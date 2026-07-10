use aegis_browser::BrowserBridgeTool;
use aegis_core::aegis_tools::{Tool, ToolContext};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

pub struct BrowserBridgeAdapter {
    inner: BrowserBridgeTool,
}

impl BrowserBridgeAdapter {
    pub fn new(port: u16) -> Self {
        Self {
            inner: BrowserBridgeTool::new(port),
        }
    }
}

#[async_trait]
impl Tool for BrowserBridgeAdapter {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> &str {
        self.inner.description()
    }

    fn parameters(&self) -> Value {
        self.inner.parameters()
    }

    async fn execute(&self, args: Value, ctx: &ToolContext<'_>) -> Result<String> {
        self.inner.execute(args, ctx.approve_fn, ctx.yolo).await
    }
}
