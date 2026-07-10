use std::collections::HashMap;
use std::sync::Arc;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

/// 处理器间共享的可变上下文
pub struct AgentContext {
    pub messages: Vec<serde_json::Value>,
    pub system_prompt_parts: Vec<String>,
    pub tools_schema: Option<Value>,
    pub metadata: HashMap<String, Value>,
    pub session_id: String,
}

impl AgentContext {
    /// Create a new empty agent context for the given session.
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            messages: vec![],
            system_prompt_parts: vec![],
            tools_schema: None,
            metadata: HashMap::new(),
            session_id: session_id.into(),
        }
    }
}

/// Request 处理器：在 LLM 调用前执行
#[async_trait]
pub trait RequestProcessor: Send + Sync {
    async fn process(&self, ctx: &mut AgentContext) -> Result<()>;
    fn name(&self) -> &str;
}

/// Response 处理器：在 LLM 调用后执行
#[async_trait]
pub trait ResponseProcessor: Send + Sync {
    async fn process(&self, ctx: &mut AgentContext, response: &mut Value) -> Result<()>;
    fn name(&self) -> &str;
}

/// 处理器管线组装器
pub struct ProcessorPipeline {
    request_processors: Vec<Arc<dyn RequestProcessor>>,
    response_processors: Vec<Arc<dyn ResponseProcessor>>,
}

impl ProcessorPipeline {
    /// Create an empty processor pipeline with no processors.
    pub fn new() -> Self {
        Self {
            request_processors: vec![],
            response_processors: vec![],
        }
    }

    /// Append a request processor to the pipeline (builder pattern).
    pub fn add_request_processor(mut self, p: Arc<dyn RequestProcessor>) -> Self {
        self.request_processors.push(p);
        self
    }

    /// Append a response processor to the pipeline (builder pattern).
    pub fn add_response_processor(mut self, p: Arc<dyn ResponseProcessor>) -> Self {
        self.response_processors.push(p);
        self
    }

    /// Run all registered request processors in order. Stops on first error.
    pub async fn run_request_processors(&self, ctx: &mut AgentContext) -> Result<()> {
        for p in &self.request_processors {
            tracing::debug!(processor = p.name(), "running request processor");
            p.process(ctx).await?;
        }
        Ok(())
    }

    /// Run all registered response processors in order. Stops on first error.
    pub async fn run_response_processors(&self, ctx: &mut AgentContext, response: &mut Value) -> Result<()> {
        for p in &self.response_processors {
            tracing::debug!(processor = p.name(), "running response processor");
            p.process(ctx, response).await?;
        }
        Ok(())
    }
}

impl Default for ProcessorPipeline {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agent_context_new() {
        let ctx = AgentContext::new("session-1");
        assert_eq!(ctx.session_id, "session-1");
        assert!(ctx.messages.is_empty());
        assert!(ctx.system_prompt_parts.is_empty());
        assert!(ctx.tools_schema.is_none());
        assert!(ctx.metadata.is_empty());
    }

    #[test]
    fn test_agent_context_new_with_string() {
        let ctx = AgentContext::new(String::from("s2"));
        assert_eq!(ctx.session_id, "s2");
    }

    struct TestRequestProcessor {
        name_val: String,
    }
    #[async_trait]
    impl RequestProcessor for TestRequestProcessor {
        async fn process(&self, ctx: &mut AgentContext) -> Result<()> {
            ctx.system_prompt_parts.push(format!("processed by {}", self.name_val));
            Ok(())
        }
        fn name(&self) -> &str { &self.name_val }
    }

    struct TestResponseProcessor;
    #[async_trait]
    impl ResponseProcessor for TestResponseProcessor {
        async fn process(&self, ctx: &mut AgentContext, response: &mut Value) -> Result<()> {
            ctx.metadata.insert("processed".into(), Value::Bool(true));
            *response = Value::String("modified".into());
            Ok(())
        }
        fn name(&self) -> &str { "test-response" }
    }

    struct FailingProcessor;
    #[async_trait]
    impl RequestProcessor for FailingProcessor {
        async fn process(&self, _ctx: &mut AgentContext) -> Result<()> {
            anyhow::bail!("intentional failure")
        }
        fn name(&self) -> &str { "failing" }
    }

    #[tokio::test]
    async fn test_pipeline_empty() {
        let pipeline = ProcessorPipeline::new();
        let mut ctx = AgentContext::new("s1");
        pipeline.run_request_processors(&mut ctx).await.unwrap();
        let mut resp = Value::Null;
        pipeline.run_response_processors(&mut ctx, &mut resp).await.unwrap();
        assert!(ctx.system_prompt_parts.is_empty());
        assert_eq!(resp, Value::Null);
    }

    #[tokio::test]
    async fn test_pipeline_default() {
        let pipeline = ProcessorPipeline::default();
        let mut ctx = AgentContext::new("s1");
        pipeline.run_request_processors(&mut ctx).await.unwrap();
        assert!(ctx.system_prompt_parts.is_empty());
    }

    #[tokio::test]
    async fn test_pipeline_request_processor() {
        let pipeline = ProcessorPipeline::new()
            .add_request_processor(Arc::new(TestRequestProcessor { name_val: "rp1".into() }));
        let mut ctx = AgentContext::new("s1");
        pipeline.run_request_processors(&mut ctx).await.unwrap();
        assert_eq!(ctx.system_prompt_parts.len(), 1);
        assert_eq!(ctx.system_prompt_parts[0], "processed by rp1");
    }

    #[tokio::test]
    async fn test_pipeline_response_processor() {
        let pipeline = ProcessorPipeline::new()
            .add_response_processor(Arc::new(TestResponseProcessor));
        let mut ctx = AgentContext::new("s1");
        let mut resp = Value::Null;
        pipeline.run_response_processors(&mut ctx, &mut resp).await.unwrap();
        assert_eq!(ctx.metadata.get("processed").unwrap(), &Value::Bool(true));
        assert_eq!(resp, Value::String("modified".into()));
    }

    #[tokio::test]
    async fn test_pipeline_chained_request_processors() {
        let pipeline = ProcessorPipeline::new()
            .add_request_processor(Arc::new(TestRequestProcessor { name_val: "first".into() }))
            .add_request_processor(Arc::new(TestRequestProcessor { name_val: "second".into() }));
        let mut ctx = AgentContext::new("s1");
        pipeline.run_request_processors(&mut ctx).await.unwrap();
        assert_eq!(ctx.system_prompt_parts.len(), 2);
        assert_eq!(ctx.system_prompt_parts[0], "processed by first");
        assert_eq!(ctx.system_prompt_parts[1], "processed by second");
    }

    #[tokio::test]
    async fn test_pipeline_request_processor_failure_stops() {
        let pipeline = ProcessorPipeline::new()
            .add_request_processor(Arc::new(FailingProcessor))
            .add_request_processor(Arc::new(TestRequestProcessor { name_val: "never".into() }));
        let mut ctx = AgentContext::new("s1");
        let result = pipeline.run_request_processors(&mut ctx).await;
        assert!(result.is_err());
        // Second processor should not have run
        assert!(ctx.system_prompt_parts.is_empty());
    }

    #[tokio::test]
    async fn test_pipeline_builder_chaining() {
        let pipeline = ProcessorPipeline::new()
            .add_request_processor(Arc::new(TestRequestProcessor { name_val: "rp".into() }))
            .add_response_processor(Arc::new(TestResponseProcessor));
        let mut ctx = AgentContext::new("s1");
        pipeline.run_request_processors(&mut ctx).await.unwrap();
        let mut resp = Value::Null;
        pipeline.run_response_processors(&mut ctx, &mut resp).await.unwrap();
        assert_eq!(ctx.system_prompt_parts.len(), 1);
        assert_eq!(resp, Value::String("modified".into()));
    }
}
