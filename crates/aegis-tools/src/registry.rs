use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// Context passed to every tool execution.
pub struct ToolContext<'a> {
    pub cwd: PathBuf,
    pub session_id: String,
    /// Callback: returns true if user approves the command.
    pub approve_fn: &'a (dyn Fn(&str) -> bool + Send + Sync),
    /// Whether to skip all approval (YOLO mode).
    pub yolo: bool,
    /// Who is invoking this tool call. `None` means "assume `LocalOwner`"
    /// — the default for legacy callers that predate the identity system.
    /// New callers should always populate this.
    pub identity: Option<aegis_security::Identity>,
    /// Whether the sandbox layer is enabled at runtime (from
    /// `[sandbox] enabled` in config.toml). When `false`, tools skip the
    /// pre_exec hook regardless of identity — this matches the "opt-in"
    /// design promise so upgrading users see no behavior change.
    pub sandbox_enabled: bool,
}

impl ToolContext<'_> {
    /// Check whether the given command is approved, either by YOLO mode or the approval callback.
    pub fn approve(&self, command: &str) -> bool {
        self.yolo || (self.approve_fn)(command)
    }

    /// Return the effective identity for this tool call, defaulting to
    /// [`aegis_security::Identity::LocalOwner`] when unset.
    pub fn effective_identity(&self) -> aegis_security::Identity {
        self.identity
            .clone()
            .unwrap_or(aegis_security::Identity::LocalOwner)
    }
}

/// A tool that the agent can invoke.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> Value;
    async fn execute(&self, args: Value, ctx: &ToolContext<'_>) -> Result<String>;
}

/// Registry of available tools.
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// Create an empty tool registry.
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    /// Register a tool, keyed by its name. Overwrites any existing tool with the same name.
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    /// Look up a registered tool by name.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }

    /// Return the names of all registered tools.
    pub fn names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    /// Serialize all registered tools into an OpenAI-compatible function-calling schema.
    pub fn to_openai_schema(&self) -> Value {
        let arr: Vec<Value> = self
            .tools
            .values()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t.name(),
                        "description": t.description(),
                        "parameters": t.parameters(),
                    }
                })
            })
            .collect();
        Value::Array(arr)
    }

    /// Return a human-readable, sorted list of tool names and their descriptions.
    pub fn tool_descriptions(&self) -> String {
        let mut names: Vec<_> = self.tools.keys().collect();
        names.sort();
        names
            .iter()
            .map(|n| {
                let t = &self.tools[n.as_str()];
                format!("- {}: {}", t.name(), t.description())
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DummyTool;

    #[async_trait]
    impl Tool for DummyTool {
        fn name(&self) -> &str {
            "dummy"
        }
        fn description(&self) -> &str {
            "A test tool"
        }
        fn parameters(&self) -> Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        async fn execute(&self, _args: Value, _ctx: &ToolContext<'_>) -> Result<String> {
            Ok("ok".into())
        }
    }

    #[test]
    fn test_register_and_get() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(DummyTool));
        assert!(reg.get("dummy").is_some());
        assert!(reg.get("nonexistent").is_none());
    }

    #[test]
    fn test_names() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(DummyTool));
        let names = reg.names();
        assert!(names.contains(&"dummy".to_string()));
    }

    #[test]
    fn test_openai_schema() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(DummyTool));
        let schema = reg.to_openai_schema();
        let arr = schema.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["type"], "function");
        assert_eq!(arr[0]["function"]["name"], "dummy");
    }

    #[test]
    fn test_tool_descriptions() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(DummyTool));
        let desc = reg.tool_descriptions();
        assert!(desc.contains("dummy: A test tool"));
    }
}
