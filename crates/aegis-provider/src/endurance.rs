//! Endurance wrapper: keep probing a rate-limited / quota-exhausted provider on
//! a fixed interval until it recovers, then carry on.
//!
//! Motivation: on a fixed token plan (e.g. MiniMax) issuing a request has no
//! marginal cost, and a rejected request does not consume the usage window. So
//! when we hit a bare `429` with no reset time, the simplest robust recovery is
//! not to guess the reset window — it is to just retry every couple of minutes
//! until a probe succeeds. This wrapper makes that behavior transparent to the
//! agent: `chat()` simply "takes longer" while quota is exhausted, then returns
//! normally once it clears.
//!
//! See `devdocs/design-quota-aware-endurance.md` §9.

use crate::traits::{Provider, StreamResult};
use aegis_types::message::{LlmResponse, Message};
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Default cadence for probing a rate-limited / quota-exhausted provider.
pub const DEFAULT_PROBE_INTERVAL: Duration = Duration::from_secs(120);

/// Returns true when an error looks like a rate-limit / quota exhaustion that
/// will clear on its own if we simply wait and retry.
///
/// Hard errors (insufficient balance, auth, bad request) are excluded so the
/// wrapper never loops forever on something a retry cannot fix. Detection is by
/// message string, matching the existing `fallback::is_fallback_error` idiom —
/// this covers both our own `ProviderError` `Display` output
/// (`"quota exhausted"`, `"rate limited"`) and generic upstream HTTP signals.
fn is_quota_wait_error(err: &anyhow::Error) -> bool {
    let msg = format!("{err:#}").to_lowercase();
    if msg.contains("insufficient balance") {
        return false;
    }
    msg.contains("quota exhausted")
        || msg.contains("rate limited")
        || msg.contains("rate limit")
        || msg.contains("429")
}

/// Wraps any [`Provider`], transparently waiting out rate-limit / quota errors
/// by re-issuing the request on a fixed interval until it succeeds.
pub struct EnduringProvider {
    inner: Arc<dyn Provider>,
    probe_interval: Duration,
    /// Optional cap on cumulative wait for a single call. `None` = wait forever.
    max_total_wait: Option<Duration>,
}

impl EnduringProvider {
    /// Wrap `inner` with the default 2-minute probe interval and no wait cap.
    pub fn new(inner: Arc<dyn Provider>) -> Self {
        Self {
            inner,
            probe_interval: DEFAULT_PROBE_INTERVAL,
            max_total_wait: None,
        }
    }

    /// Override the probe interval (how often to re-try while rate-limited).
    pub fn with_probe_interval(mut self, interval: Duration) -> Self {
        self.probe_interval = interval;
        self
    }

    /// Set an optional cap on total wait per call (`None` = unbounded).
    pub fn with_max_total_wait(mut self, max: Option<Duration>) -> Self {
        self.max_total_wait = max;
        self
    }

    /// Whether we may keep waiting given how long we have already waited.
    fn within_budget(&self, elapsed: Duration) -> bool {
        match self.max_total_wait {
            Some(max) => elapsed < max,
            None => true,
        }
    }
}

#[async_trait]
impl Provider for EnduringProvider {
    fn name(&self) -> &str {
        self.inner.name()
    }

    async fn chat(
        &self,
        messages: &[Message],
        tools: Option<&serde_json::Value>,
    ) -> Result<LlmResponse> {
        let start = Instant::now();
        loop {
            match self.inner.chat(messages, tools).await {
                Ok(resp) => return Ok(resp),
                Err(e) => {
                    if !is_quota_wait_error(&e) || !self.within_budget(start.elapsed()) {
                        return Err(e);
                    }
                    tracing::warn!(
                        "quota/rate limited; probing again in {}s: {e:#}",
                        self.probe_interval.as_secs()
                    );
                    tokio::time::sleep(self.probe_interval).await;
                }
            }
        }
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: Option<&serde_json::Value>,
    ) -> Result<StreamResult> {
        let start = Instant::now();
        loop {
            match self.inner.chat_stream(messages, tools).await {
                Ok(stream) => return Ok(stream),
                Err(e) => {
                    if !is_quota_wait_error(&e) || !self.within_budget(start.elapsed()) {
                        return Err(e);
                    }
                    tracing::warn!(
                        "quota/rate limited (stream); probing again in {}s: {e:#}",
                        self.probe_interval.as_secs()
                    );
                    tokio::time::sleep(self.probe_interval).await;
                }
            }
        }
    }

    fn supports_streaming(&self) -> bool {
        self.inner.supports_streaming()
    }

    fn supports_tools(&self) -> bool {
        self.inner.supports_tools()
    }

    fn supports_prompt_caching(&self) -> bool {
        self.inner.supports_prompt_caching()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::StreamEvent;
    use aegis_types::message::{Content, Role};
    use futures::stream;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Fails `fail_times` times with `err`, then succeeds.
    struct FlakyProvider {
        calls: AtomicU32,
        fail_times: u32,
        err: &'static str,
    }

    impl FlakyProvider {
        fn new(fail_times: u32, err: &'static str) -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicU32::new(0),
                fail_times,
                err,
            })
        }
        fn call_count(&self) -> u32 {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Provider for FlakyProvider {
        fn name(&self) -> &str {
            "flaky"
        }
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: Option<&serde_json::Value>,
        ) -> Result<LlmResponse> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n < self.fail_times {
                Err(anyhow::anyhow!("{}", self.err))
            } else {
                Ok(LlmResponse {
                    message: Message {
                        role: Role::Assistant,
                        content: Some(Content::Text("ok".into())),
                        tool_calls: None,
                        tool_call_id: None,
                        name: None,
                        reasoning: None,
                    },
                    finish_reason: None,
                    usage: None,
                })
            }
        }
        async fn chat_stream(
            &self,
            _messages: &[Message],
            _tools: Option<&serde_json::Value>,
        ) -> Result<StreamResult> {
            let events: Vec<Result<StreamEvent>> = vec![];
            Ok(Box::pin(stream::iter(events)))
        }
    }

    fn msgs() -> Vec<Message> {
        vec![Message::user("hi")]
    }

    #[tokio::test]
    async fn recovers_after_probing_through_quota_errors() {
        let flaky = FlakyProvider::new(2, "quota exhausted (retry after Some(300)s)");
        let p = EnduringProvider::new(flaky.clone())
            .with_probe_interval(Duration::from_millis(1));
        let r = p.chat(&msgs(), None).await.unwrap();
        assert_eq!(r.message.text(), "ok");
        // 2 failed probes + 1 success.
        assert_eq!(flaky.call_count(), 3);
    }

    #[tokio::test]
    async fn stops_immediately_on_hard_error() {
        let flaky = FlakyProvider::new(100, "insufficient balance");
        let p = EnduringProvider::new(flaky.clone())
            .with_probe_interval(Duration::from_millis(1));
        let r = p.chat(&msgs(), None).await;
        assert!(r.is_err());
        // No probing on a hard error.
        assert_eq!(flaky.call_count(), 1);
    }

    #[tokio::test]
    async fn gives_up_after_max_total_wait() {
        let flaky = FlakyProvider::new(100, "429 rate limited");
        let p = EnduringProvider::new(flaky.clone())
            .with_probe_interval(Duration::from_millis(10))
            .with_max_total_wait(Some(Duration::from_millis(5)));
        let r = p.chat(&msgs(), None).await;
        assert!(r.is_err());
    }

    #[test]
    fn quota_wait_detection() {
        assert!(is_quota_wait_error(&anyhow::anyhow!(
            "quota exhausted (retry after Some(300)s)"
        )));
        assert!(is_quota_wait_error(&anyhow::anyhow!(
            "rate limited (retry after None s)"
        )));
        assert!(is_quota_wait_error(&anyhow::anyhow!("HTTP 429 Too Many Requests")));
        assert!(!is_quota_wait_error(&anyhow::anyhow!("insufficient balance")));
        assert!(!is_quota_wait_error(&anyhow::anyhow!("401 unauthorized")));
    }
}
