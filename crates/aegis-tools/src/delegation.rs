/// Multi-agent delegation and collaboration tools.
///
/// Implements:
/// - DelegateWorkTool  (#1): Agent discovers and delegates to coworkers via tool description
/// - AskQuestionTool   (#2): Ask a coworker a question without full delegation
/// - ToolConfig        (#12, #13): max_usage_count + result_as_answer
/// - Guardrail         (#14, #15): validate task output with retry
/// - ConditionalTask   (#11): execute task only if condition on previous output passes
use crate::registry::{Tool, ToolContext};
use aegis_a2a::client::A2AClient;
use aegis_a2a::types::{Message, MessageRole, Part, TaskGetParams, TaskSendParams};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

/// Maximum A2A delegation hop count before a task is refused (loop protection).
pub const MAX_A2A_HOPS: u64 = 4;

/// True if `url` points at this process's own A2A endpoint (set via
/// `AEGIS_A2A_SELF` by `aegis a2a`). Prevents the same-machine self-delegation
/// loop where a peer's URL is the very server executing the task.
pub fn is_self_url(url: &str) -> bool {
    let norm = |u: &str| u.trim().trim_end_matches('/').to_ascii_lowercase();
    std::env::var("AEGIS_A2A_SELF")
        .ok()
        .map(|s| norm(&s) == norm(url))
        .unwrap_or(false)
}

/// Current delegation depth of the task this process is executing (set by the
/// A2A server from the incoming task's `aegis_hops`); 0 for a top-level CLI.
fn current_hops() -> u64 {
    std::env::var("AEGIS_A2A_DEPTH")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Metadata to attach when submitting a delegated task: the next hop count.
fn next_hops_metadata() -> Option<std::collections::HashMap<String, Value>> {
    let mut m = std::collections::HashMap::new();
    m.insert("aegis_hops".to_string(), json!(current_hops() + 1));
    Some(m)
}

/// Refusal message if delegating to `url` would loop (self) or exceed the hop
/// cap; `None` means it's safe to proceed.
fn delegation_block_reason(url: &str) -> Option<String> {
    if is_self_url(url) {
        return Some(format!(
            "Refused: '{url}' is this agent's own A2A endpoint — delegating to self would loop."
        ));
    }
    if current_hops() + 1 > MAX_A2A_HOPS {
        return Some(format!(
            "Refused: A2A delegation hop limit ({MAX_A2A_HOPS}) reached — aborting to avoid a delegation loop."
        ));
    }
    None
}

// ═══════════════════════════════════════════
// AgentInfo — lightweight description of a peer agent
// ═══════════════════════════════════════════

/// Minimal description of a peer agent, used for discovery.
pub struct AgentInfo {
    pub name: String,
    pub role: String,
    pub expertise: String,
    /// A2A endpoint URL. If non-empty, delegation uses A2AClient.
    pub url: String,
    /// Callback that the delegation tool will invoke to execute the task.
    /// Returns the agent's response as a String.
    pub executor: Arc<dyn Fn(String, String) -> futures::future::BoxFuture<'static, Result<String>> + Send + Sync>,
}

impl AgentInfo {
    /// Create a new AgentInfo with a simple async executor closure.
    pub fn new(
        name: impl Into<String>,
        role: impl Into<String>,
        expertise: impl Into<String>,
        executor: Arc<dyn Fn(String, String) -> futures::future::BoxFuture<'static, Result<String>> + Send + Sync>,
    ) -> Self {
        Self {
            name: name.into(),
            role: role.into(),
            expertise: expertise.into(),
            url: String::new(),
            executor,
        }
    }
}

// ═══════════════════════════════════════════
// DelegateWorkTool (#1)
// ═══════════════════════════════════════════

/// Delegate a subtask to a coworker agent, discovered by name.
/// The calling agent does not hold a direct reference to its peers;
/// it discovers them through the tool description (decoupling).
pub struct DelegateWorkTool {
    pub available_agents: Vec<AgentInfo>,
}

impl DelegateWorkTool {
    /// Create a new `DelegateWorkTool` with the given list of available agents.
    pub fn new(agents: Vec<AgentInfo>) -> Self {
        Self { available_agents: agents }
    }

    fn find_agent(&self, name: &str) -> Option<&AgentInfo> {
        self.available_agents
            .iter()
            .find(|a| a.name.eq_ignore_ascii_case(name))
    }

    fn agent_list(&self) -> String {
        let mut lines: Vec<String> = self
            .available_agents
            .iter()
            .map(|a| format!("- {} ({}): {}", a.name, a.role, a.expertise))
            .collect();
        // Merge dynamically-registered peers (peer tool / peers.json).
        for p in crate::peers::list() {
            if !self.available_agents.iter().any(|a| a.name.eq_ignore_ascii_case(&p.name)) {
                lines.push(format!("- {} ({}): {}", p.name, p.role, p.expertise));
            }
        }
        lines.join("\n")
    }

    /// Poll a previously submitted task by ID.
    pub async fn poll_task(task_id: &str, agent_url: &str) -> Result<String> {
        let client = A2AClient::new(agent_url);
        let task = client
            .get(TaskGetParams {
                id: task_id.to_string(),
                history_length: None,
            })
            .await?;
        Ok(format!(
            "Task {} — state: {:?}",
            task.id, task.status.state
        ))
    }
}

#[async_trait]
impl Tool for DelegateWorkTool {
    fn name(&self) -> &str {
        "delegate_work"
    }

    fn description(&self) -> &str {
        "Delegate a task to a coworker agent. \
         Specify: coworker name, task description, and context. \
         Available coworkers are listed in the parameters schema."
    }

    fn parameters(&self) -> Value {
        let agent_names: Vec<Value> = {
            let mut names: Vec<Value> = self
                .available_agents
                .iter()
                .map(|a| Value::String(a.name.clone()))
                .collect();
            for p in crate::peers::list() {
                if !self.available_agents.iter().any(|a| a.name.eq_ignore_ascii_case(&p.name)) {
                    names.push(Value::String(p.name.clone()));
                }
            }
            names
        };
        let agent_roster = self.agent_list();
        json!({
            "type": "object",
            "properties": {
                "coworker": {
                    "type": "string",
                    "description": format!("Name of the coworker to delegate to. Available:\n{}", agent_roster),
                    "enum": agent_names
                },
                "task": {
                    "type": "string",
                    "description": "Detailed description of the task to delegate"
                },
                "context": {
                    "type": "string",
                    "description": "Background context the coworker needs to complete the task"
                }
            },
            "required": ["coworker", "task"]
        })
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext<'_>) -> Result<String> {
        let coworker = args["coworker"]
            .as_str()
            .ok_or_else(|| anyhow!("'coworker' is required"))?;
        let task_desc = args["task"]
            .as_str()
            .ok_or_else(|| anyhow!("'task' is required"))?;
        let context = args["context"].as_str().unwrap_or("");

        let agent = self.find_agent(coworker);

        // Static roster agent (may have a local executor) — original path.
        if let Some(agent) = agent {
            if !agent.url.is_empty() {
                if let Some(reason) = delegation_block_reason(&agent.url) {
                    return Ok(reason);
                }
                let client = A2AClient::new(&agent.url);
                let text = if context.is_empty() {
                    task_desc.to_string()
                } else {
                    format!("Context: {}\n\nTask: {}", context, task_desc)
                };
                let params = TaskSendParams {
                    id: None,
                    message: None,
                    messages: vec![Message {
                        role: MessageRole::User,
                        parts: vec![Part::Text { text }],
                        kind: "message".into(),
                        message_id: None,
                        context_id: None,
                        task_id: None,
                        metadata: None,
                    }],
                    metadata: next_hops_metadata(),
                    session_id: None,
                };
                match client.submit(params).await {
                    Ok(task) => Ok(format!(
                        "[{} delegated via A2A] task_id={} state={:?}",
                        agent.name, task.id, task.status.state
                    )),
                    Err(e) => {
                        tracing::warn!(
                            "A2A delegation to {} failed ({}), falling back to executor",
                            agent.name,
                            e
                        );
                        let result = (agent.executor)(task_desc.to_string(), context.to_string()).await?;
                        Ok(format!("[{} completed task]\n{}", agent.name, result))
                    }
                }
            } else {
                let result = (agent.executor)(task_desc.to_string(), context.to_string()).await?;
                Ok(format!("[{} completed task]\n{}", agent.name, result))
            }
        } else if let Some(peer) = crate::peers::get(coworker) {
            // Dynamically-registered peer (peers.json) — A2A only.
            if let Some(reason) = delegation_block_reason(&peer.url) {
                return Ok(reason);
            }
            let mut client = A2AClient::new(&peer.url);
            if let Some(t) = &peer.token {
                client = client.with_bearer_token(t.clone());
            }
            let text = if context.is_empty() {
                task_desc.to_string()
            } else {
                format!("Context: {}\n\nTask: {}", context, task_desc)
            };
            let params = TaskSendParams {
                id: None,
                message: None,
                messages: vec![Message {
                    role: MessageRole::User,
                    parts: vec![Part::Text { text }],
                    kind: "message".into(),
                    message_id: None,
                    context_id: None,
                    task_id: None,
                    metadata: None,
                }],
                metadata: next_hops_metadata(),
                session_id: None,
            };
            match client.submit(params).await {
                Ok(task) => Ok(format!(
                    "[{} delegated via A2A] task_id={} state={:?}",
                    peer.name, task.id, task.status.state
                )),
                Err(e) => Ok(format!("[{}] A2A delegation failed: {e}", peer.name)),
            }
        } else {
            Err(anyhow!(
                "Unknown coworker '{coworker}'. Available:\n{}",
                self.agent_list()
            ))
        }
    }
}

// ═══════════════════════════════════════════
// AskQuestionTool (#2)
// ═══════════════════════════════════════════

/// Ask a coworker a specific question and get their answer.
/// Unlike DelegateWorkTool, the calling agent retains control and
/// uses the answer to inform its own decision.
pub struct AskQuestionTool {
    pub available_agents: Vec<AgentInfo>,
}

impl AskQuestionTool {
    /// Create a new `AskQuestionTool` with the given list of available agents.
    pub fn new(agents: Vec<AgentInfo>) -> Self {
        Self { available_agents: agents }
    }

    fn find_agent(&self, name: &str) -> Option<&AgentInfo> {
        self.available_agents
            .iter()
            .find(|a| a.name.eq_ignore_ascii_case(name))
    }

    fn agent_list(&self) -> String {
        let mut lines: Vec<String> = self
            .available_agents
            .iter()
            .map(|a| format!("- {} ({}): {}", a.name, a.role, a.expertise))
            .collect();
        // Merge dynamically-registered peers (peer tool / peers.json).
        for p in crate::peers::list() {
            if !self.available_agents.iter().any(|a| a.name.eq_ignore_ascii_case(&p.name)) {
                lines.push(format!("- {} ({}): {}", p.name, p.role, p.expertise));
            }
        }
        lines.join("\n")
    }
}

#[async_trait]
impl Tool for AskQuestionTool {
    fn name(&self) -> &str {
        "ask_question"
    }

    fn description(&self) -> &str {
        "Ask a coworker agent a specific question to gather information. \
         Unlike delegate_work, you retain control and will use the answer yourself. \
         Use this when you need a peer's expertise on a narrow question."
    }

    fn parameters(&self) -> Value {
        let agent_names: Vec<Value> = {
            let mut names: Vec<Value> = self
                .available_agents
                .iter()
                .map(|a| Value::String(a.name.clone()))
                .collect();
            for p in crate::peers::list() {
                if !self.available_agents.iter().any(|a| a.name.eq_ignore_ascii_case(&p.name)) {
                    names.push(Value::String(p.name.clone()));
                }
            }
            names
        };
        let agent_roster = self.agent_list();
        json!({
            "type": "object",
            "properties": {
                "coworker": {
                    "type": "string",
                    "description": format!("Name of the coworker to ask. Available:\n{}", agent_roster),
                    "enum": agent_names
                },
                "question": {
                    "type": "string",
                    "description": "The specific question you want answered"
                },
                "context": {
                    "type": "string",
                    "description": "Background context that helps the coworker understand the question"
                }
            },
            "required": ["coworker", "question"]
        })
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext<'_>) -> Result<String> {
        let coworker = args["coworker"]
            .as_str()
            .ok_or_else(|| anyhow!("'coworker' is required"))?;
        let question = args["question"]
            .as_str()
            .ok_or_else(|| anyhow!("'question' is required"))?;
        let context = args["context"].as_str().unwrap_or("");

        // Dynamically-registered peer (peers.json): use non-streaming submit
        // (our A2A server doesn't stream) and return the completed answer.
        if self.find_agent(coworker).is_none() {
            if let Some(peer) = crate::peers::get(coworker) {
                if let Some(reason) = delegation_block_reason(&peer.url) {
                    return Ok(reason);
                }
                let mut client = A2AClient::new(&peer.url);
                if let Some(t) = &peer.token {
                    client = client.with_bearer_token(t.clone());
                }
                let text = if context.is_empty() {
                    format!("Please answer this question concisely: {question}")
                } else {
                    format!("Context: {context}\n\nPlease answer this question concisely: {question}")
                };
                let params = TaskSendParams {
                    id: None,
                    message: None,
                    messages: vec![Message {
                        role: MessageRole::User,
                        parts: vec![Part::Text { text }],
                        kind: "message".into(),
                        message_id: None,
                        context_id: None,
                        task_id: None,
                        metadata: None,
                    }],
                    metadata: next_hops_metadata(),
                    session_id: None,
                };
                return match client.submit(params).await {
                    Ok(task) => {
                        let ans = task
                            .status
                            .message
                            .and_then(|m| {
                                m.parts.into_iter().find_map(|p| match p {
                                    Part::Text { text } => Some(text),
                                    _ => None,
                                })
                            })
                            .unwrap_or_else(|| format!("(submitted, state={:?})", task.status.state));
                        Ok(format!("[{} answers]\n{}", peer.name, ans))
                    }
                    Err(e) => Ok(format!("[{}] A2A ask failed: {e}", peer.name)),
                };
            }
        }

        let agent = self
            .find_agent(coworker)
            .ok_or_else(|| anyhow!("Unknown coworker '{coworker}'. Available: {}", self.agent_list()))?;

        // If agent has a URL, use A2AClient streaming; otherwise fall back to executor
        if !agent.url.is_empty() {
            if let Some(reason) = delegation_block_reason(&agent.url) {
                return Ok(reason);
            }
            let client = A2AClient::new(&agent.url);
            let text = if context.is_empty() {
                format!("Please answer this question concisely: {}", question)
            } else {
                format!(
                    "Context: {}\n\nPlease answer this question concisely: {}",
                    context, question
                )
            };
            let params = TaskSendParams {
                id: None,
                message: None,
                messages: vec![Message {
                    role: MessageRole::User,
                    parts: vec![Part::Text { text }],
                    kind: "message".into(),
                    message_id: None,
                    context_id: None,
                    task_id: None,
                    metadata: None,
                }],
                metadata: next_hops_metadata(),
                session_id: None,
            };
            match client.subscribe(params).await {
                Ok(mut stream) => {
                    use futures::StreamExt;
                    // Wait for the first event that carries text
                    while let Some(event_result) = stream.next().await {
                        match event_result {
                            Ok(aegis_a2a::types::TaskEvent::StatusUpdate(ev)) => {
                                if let Some(msg) = &ev.status.message {
                                    for part in &msg.parts {
                                        if let Part::Text { text } = part {
                                            return Ok(format!("[{} answers]\n{}", agent.name, text));
                                        }
                                    }
                                }
                            }
                            Ok(aegis_a2a::types::TaskEvent::ArtifactUpdate(ev)) => {
                                for part in &ev.artifact.parts {
                                    if let Part::Text { text } = part {
                                        return Ok(format!("[{} answers]\n{}", agent.name, text));
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "A2A stream from {} failed ({}), falling back to executor",
                                    agent.name,
                                    e
                                );
                                break;
                            }
                        }
                    }
                    // Fallback if stream ended without text
                    let prompt = if context.is_empty() {
                        format!("Please answer this question concisely: {}", question)
                    } else {
                        format!(
                            "Context: {}\n\nPlease answer this question concisely: {}",
                            context, question
                        )
                    };
                    let answer = (agent.executor)(prompt, String::new()).await?;
                    Ok(format!("[{} answers]\n{}", agent.name, answer))
                }
                Err(e) => {
                    tracing::warn!(
                        "A2A subscribe to {} failed ({}), falling back to executor",
                        agent.name,
                        e
                    );
                    let prompt = if context.is_empty() {
                        format!("Please answer this question concisely: {}", question)
                    } else {
                        format!(
                            "Context: {}\n\nPlease answer this question concisely: {}",
                            context, question
                        )
                    };
                    let answer = (agent.executor)(prompt, String::new()).await?;
                    Ok(format!("[{} answers]\n{}", agent.name, answer))
                }
            }
        } else {
            // No URL — use executor directly (fallback / simulation)
            let prompt = if context.is_empty() {
                format!("Please answer this question concisely: {}", question)
            } else {
                format!(
                    "Context: {}\n\nPlease answer this question concisely: {}",
                    context, question
                )
            };
            let answer = (agent.executor)(prompt, String::new()).await?;
            Ok(format!("[{} answers]\n{}", agent.name, answer))
        }
    }
}

// ═══════════════════════════════════════════
// ToolConfig (#12, #13)
// ═══════════════════════════════════════════

/// Configuration that wraps a Tool to add usage limits and short-circuit behaviour.
pub struct ToolConfig {
    inner: Arc<dyn Tool>,
    /// If Some(n), the tool is disabled after n successful calls.
    pub max_usage_count: Option<u32>,
    /// If true, the tool's output is used directly as the final answer,
    /// bypassing further LLM reasoning in the agent loop.
    pub result_as_answer: bool,
    usage_count: Mutex<u32>,
}

impl ToolConfig {
    /// Wrap a tool with default configuration (no usage limit, no result-as-answer).
    pub fn new(tool: Arc<dyn Tool>) -> Self {
        Self {
            inner: tool,
            max_usage_count: None,
            result_as_answer: false,
            usage_count: Mutex::new(0),
        }
    }

    /// Set the maximum number of times this tool may be called before it is disabled.
    pub fn with_max_usage(mut self, n: u32) -> Self {
        self.max_usage_count = Some(n);
        self
    }

    /// Mark this tool so that its output is treated as the final agent answer.
    pub fn with_result_as_answer(mut self) -> Self {
        self.result_as_answer = true;
        self
    }

    /// Returns true when the tool has hit its usage cap.
    pub fn is_exhausted(&self) -> bool {
        match self.max_usage_count {
            None => false,
            Some(limit) => {
                let count = self.usage_count.lock().expect("lock poisoned");
                *count >= limit
            }
        }
    }
}

#[async_trait]
impl Tool for ToolConfig {
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
        if self.is_exhausted() {
            return Err(anyhow!(
                "Tool '{}' has reached its maximum usage count of {}",
                self.inner.name(),
                self.max_usage_count.unwrap_or(0)
            ));
        }

        let result = self.inner.execute(args, ctx).await?;

        {
            let mut count = self.usage_count.lock().expect("lock poisoned");
            *count += 1;
        }

        // Prefix so the agent loop can detect result_as_answer
        if self.result_as_answer {
            Ok(format!("[FINAL_ANSWER]\n{}", result))
        } else {
            Ok(result)
        }
    }
}

// ═══════════════════════════════════════════
// Guardrail (#14, #15)
// ═══════════════════════════════════════════

/// Outcome of a guardrail validation.
#[derive(Debug)]
pub enum GuardrailResult {
    Pass,
    Fail { reason: String },
}

/// A single validator in a guardrail chain.
pub trait GuardrailValidator: Send + Sync {
    fn validate(&self, output: &str) -> GuardrailResult;
}

/// A guardrail that wraps a validator closure.
pub struct FnGuardrail {
    validator: Box<dyn Fn(&str) -> GuardrailResult + Send + Sync>,
    pub max_retries: u32,
}

impl FnGuardrail {
    /// Create a new `FnGuardrail` with the given retry limit and validator closure.
    pub fn new(
        max_retries: u32,
        validator: impl Fn(&str) -> GuardrailResult + Send + Sync + 'static,
    ) -> Self {
        Self {
            validator: Box::new(validator),
            max_retries,
        }
    }
}

impl GuardrailValidator for FnGuardrail {
    fn validate(&self, output: &str) -> GuardrailResult {
        (self.validator)(output)
    }
}

/// Execute an async task producer with guardrail retry logic.
///
/// `produce` is called on each attempt; it receives optional feedback from previous
/// failed attempts so the agent can correct itself.
pub async fn execute_with_guardrail<F, Fut>(
    guardrail: &FnGuardrail,
    mut produce: F,
) -> Result<String>
where
    F: FnMut(Option<String>) -> Fut,
    Fut: std::future::Future<Output = Result<String>>,
{
    let mut feedback: Option<String> = None;
    for attempt in 0..=guardrail.max_retries {
        let output = produce(feedback.clone()).await?;
        match (guardrail.validator)(&output) {
            GuardrailResult::Pass => return Ok(output),
            GuardrailResult::Fail { reason } => {
                if attempt == guardrail.max_retries {
                    return Err(anyhow!(
                        "Guardrail failed after {} retries. Last reason: {}",
                        guardrail.max_retries,
                        reason
                    ));
                }
                feedback = Some(format!(
                    "Attempt {} failed guardrail check: {}. Please correct and try again.",
                    attempt + 1,
                    reason
                ));
            }
        }
    }
    Err(anyhow!("Guardrail: exhausted retries"))
}

/// Chain multiple guardrail validators — all must pass.
pub struct GuardrailChain {
    validators: Vec<Box<dyn GuardrailValidator>>,
}

impl Default for GuardrailChain {
    fn default() -> Self {
        Self::new()
    }
}

impl GuardrailChain {
    /// Create an empty guardrail chain.
    pub fn new() -> Self {
        Self { validators: Vec::new() }
    }

    /// Append a validator to the chain. All validators must pass for the chain to pass.
    pub fn push(mut self, v: impl GuardrailValidator + 'static) -> Self {
        self.validators.push(Box::new(v));
        self
    }
}

impl GuardrailValidator for GuardrailChain {
    fn validate(&self, output: &str) -> GuardrailResult {
        for v in &self.validators {
            match v.validate(output) {
                GuardrailResult::Pass => continue,
                fail => return fail,
            }
        }
        GuardrailResult::Pass
    }
}

// ═══════════════════════════════════════════
// ConditionalTask (#11)
// ═══════════════════════════════════════════

/// Wraps a task description with a condition that is evaluated against
/// the previous task's output. If the condition returns false, the task
/// is skipped entirely.
pub struct ConditionalTask {
    pub name: String,
    pub description: String,
    pub condition: Box<dyn Fn(&str) -> bool + Send + Sync>,
}

impl ConditionalTask {
    /// Create a new conditional task that only executes when `condition` returns true
    /// on the previous task's output.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        condition: impl Fn(&str) -> bool + Send + Sync + 'static,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            condition: Box::new(condition),
        }
    }

    /// Returns true if this task should be executed given `previous_output`.
    pub fn should_execute(&self, previous_output: &str) -> bool {
        (self.condition)(previous_output)
    }
}

// ═══════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn make_ctx() -> ToolContext<'static> {
        ToolContext {
            cwd: PathBuf::from("/tmp"),
            session_id: "test".to_string(),
            approve_fn: &|_| true,
            yolo: true,
            identity: None,
            sandbox_enabled: false,
        }
    }

    fn stub_agent(name: &str) -> AgentInfo {
        let n = name.to_string();
        AgentInfo {
            name: n.clone(),
            role: "Tester".to_string(),
            expertise: "testing".to_string(),
            url: String::new(),
            executor: Arc::new(move |task, _ctx| {
                let n = n.clone();
                Box::pin(async move { Ok(format!("{} handled: {}", n, task)) })
            }),
        }
    }

    #[tokio::test]
    async fn delegate_work_success() {
        let tool = DelegateWorkTool::new(vec![stub_agent("Alice")]);
        let ctx = make_ctx();
        let result = tool
            .execute(json!({"coworker": "Alice", "task": "write tests", "context": "Rust project"}), &ctx)
            .await
            .unwrap();
        assert!(result.contains("Alice"));
        assert!(result.contains("write tests"));
    }

    #[tokio::test]
    async fn delegate_work_unknown_agent() {
        let tool = DelegateWorkTool::new(vec![stub_agent("Alice")]);
        let ctx = make_ctx();
        let result = tool
            .execute(json!({"coworker": "Bob", "task": "something"}), &ctx)
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Unknown coworker"));
    }

    #[tokio::test]
    async fn ask_question_success() {
        let tool = AskQuestionTool::new(vec![stub_agent("Bob")]);
        let ctx = make_ctx();
        let result = tool
            .execute(json!({"coworker": "Bob", "question": "what is 2+2?"}), &ctx)
            .await
            .unwrap();
        assert!(result.contains("Bob"));
    }

    #[tokio::test]
    async fn tool_config_max_usage() {
        use crate::registry::{Tool, ToolContext};

        struct EchoTool;
        #[async_trait]
        impl Tool for EchoTool {
            fn name(&self) -> &str { "echo" }
            fn description(&self) -> &str { "echo" }
            fn parameters(&self) -> Value { json!({}) }
            async fn execute(&self, _args: Value, _ctx: &ToolContext<'_>) -> Result<String> {
                Ok("ok".to_string())
            }
        }

        let wrapped = ToolConfig::new(Arc::new(EchoTool)).with_max_usage(2);
        let ctx = make_ctx();
        assert!(wrapped.execute(json!({}), &ctx).await.is_ok()); // use 1
        assert!(wrapped.execute(json!({}), &ctx).await.is_ok()); // use 2
        assert!(wrapped.execute(json!({}), &ctx).await.is_err()); // exhausted
    }

    #[tokio::test]
    async fn tool_config_result_as_answer() {
        use crate::registry::{Tool, ToolContext};

        struct EchoTool;
        #[async_trait]
        impl Tool for EchoTool {
            fn name(&self) -> &str { "echo" }
            fn description(&self) -> &str { "echo" }
            fn parameters(&self) -> Value { json!({}) }
            async fn execute(&self, _args: Value, _ctx: &ToolContext<'_>) -> Result<String> {
                Ok("42".to_string())
            }
        }

        let wrapped = ToolConfig::new(Arc::new(EchoTool)).with_result_as_answer();
        let ctx = make_ctx();
        let out = wrapped.execute(json!({}), &ctx).await.unwrap();
        assert!(out.starts_with("[FINAL_ANSWER]"));
        assert!(out.contains("42"));
    }

    #[tokio::test]
    async fn guardrail_passes() {
        let g = FnGuardrail::new(3, |output| {
            if output.contains("good") {
                GuardrailResult::Pass
            } else {
                GuardrailResult::Fail { reason: "missing 'good'".to_string() }
            }
        });
        let mut attempts = 0u32;
        let result = execute_with_guardrail(&g, |_feedback| {
            attempts += 1;
            async move { Ok(if attempts >= 2 { "good output".to_string() } else { "bad".to_string() }) }
        })
        .await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("good"));
    }

    #[tokio::test]
    async fn guardrail_exhausted() {
        let g = FnGuardrail::new(2, |_| GuardrailResult::Fail { reason: "always fails".to_string() });
        let result = execute_with_guardrail(&g, |_| async move { Ok("bad".to_string()) }).await;
        assert!(result.is_err());
    }

    #[test]
    fn conditional_task_skip() {
        let task = ConditionalTask::new(
            "deploy",
            "Deploy to production",
            |output| output.contains("tests passed"),
        );
        assert!(!task.should_execute("tests failed"));
        assert!(task.should_execute("all tests passed successfully"));
    }

    #[test]
    fn guardrail_chain() {
        let chain = GuardrailChain::new()
            .push(FnGuardrail::new(0, |o| {
                if o.len() > 5 { GuardrailResult::Pass } else { GuardrailResult::Fail { reason: "too short".into() } }
            }))
            .push(FnGuardrail::new(0, |o| {
                if o.contains("ok") { GuardrailResult::Pass } else { GuardrailResult::Fail { reason: "missing ok".into() } }
            }));
        assert!(matches!(chain.validate("this is ok"), GuardrailResult::Pass));
        assert!(matches!(chain.validate("nope"), GuardrailResult::Fail { .. }));
        assert!(matches!(chain.validate("this is bad"), GuardrailResult::Fail { .. }));
    }
}
