use std::collections::HashMap;
use std::sync::Arc;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Graph state trait - must be merged when parallel branches complete
pub trait GraphState: Send + Sync + Clone {
    fn merge(&mut self, other: Self);
}

/// 图节点 trait
#[async_trait]
pub trait GraphNode<S: GraphState>: Send + Sync {
    async fn execute(&self, state: &mut S) -> Result<()>;
    fn name(&self) -> &str;
}

/// 图边类型
pub enum Edge<S: GraphState> {
    /// 静态边：总是跳转到指定节点
    Static { target: String },
    /// 条件边：根据状态动态路由
    Conditional {
        router: Arc<dyn Fn(&S) -> String + Send + Sync>,
    },
}

/// 特殊节点名
pub const NODE_START: &str = "__start__";
pub const NODE_END: &str = "__end__";

/// 图结构
pub struct Graph<S: GraphState> {
    nodes: HashMap<String, Arc<dyn GraphNode<S>>>,
    edges: HashMap<String, Vec<Edge<S>>>,
    entry_point: String,
    max_iterations: usize,
}

impl<S: GraphState + 'static> Graph<S> {
    /// Execute the graph starting from the entry point, running nodes in order.
    pub async fn execute(&self, state: &mut S) -> Result<()> {
        let mut current = self.entry_point.clone();
        let mut iterations = 0;

        loop {
            if current == NODE_END || iterations >= self.max_iterations {
                break;
            }
            iterations += 1;

            // 执行当前节点
            if let Some(node) = self.nodes.get(&current) {
                tracing::debug!(node = current, iteration = iterations, "executing graph node");
                node.execute(state).await?;
            } else if current != NODE_START {
                return Err(anyhow!("graph node not found: {}", current));
            }

            // 解析下一个节点
            current = self.resolve_next(&current, state)?;
        }

        if iterations >= self.max_iterations {
            tracing::warn!("graph reached max_iterations={}", self.max_iterations);
        }

        Ok(())
    }

    fn resolve_next(&self, current: &str, state: &S) -> Result<String> {
        let edges = match self.edges.get(current) {
            Some(e) => e,
            None => return Ok(NODE_END.to_string()),
        };

        if let Some(edge) = edges.first() {
            match edge {
                Edge::Static { target } => return Ok(target.clone()),
                Edge::Conditional { router } => {
                    let next = (router)(state);
                    return Ok(next);
                }
            }
        }

        Ok(NODE_END.to_string())
    }
}

/// GraphBuilder - 构建器模式
pub struct GraphBuilder<S: GraphState> {
    nodes: HashMap<String, Arc<dyn GraphNode<S>>>,
    edges: HashMap<String, Vec<Edge<S>>>,
    entry_point: Option<String>,
    max_iterations: usize,
}

impl<S: GraphState + 'static> GraphBuilder<S> {
    /// Create a new graph builder with no nodes or edges.
    pub fn new() -> Self {
        Self {
            nodes: HashMap::new(),
            edges: HashMap::new(),
            entry_point: None,
            max_iterations: 100,
        }
    }

    /// Register a named node in the graph (builder pattern).
    pub fn add_node(mut self, name: impl Into<String>, node: Arc<dyn GraphNode<S>>) -> Self {
        self.nodes.insert(name.into(), node);
        self
    }

    /// Set the entry point node for execution (builder pattern).
    pub fn set_entry(mut self, name: impl Into<String>) -> Self {
        self.entry_point = Some(name.into());
        self
    }

    /// Add a static edge from one node to another (builder pattern).
    pub fn add_edge(mut self, from: impl Into<String>, to: impl Into<String>) -> Self {
        self.edges
            .entry(from.into())
            .or_default()
            .push(Edge::Static { target: to.into() });
        self
    }

    /// Add a conditional edge that routes based on the current state (builder pattern).
    pub fn add_conditional_edge(
        mut self,
        from: impl Into<String>,
        router: impl Fn(&S) -> String + Send + Sync + 'static,
    ) -> Self {
        self.edges
            .entry(from.into())
            .or_default()
            .push(Edge::Conditional {
                router: Arc::new(router),
            });
        self
    }

    /// Set the maximum iteration count before the graph stops (builder pattern).
    pub fn max_iterations(mut self, n: usize) -> Self {
        self.max_iterations = n;
        self
    }

    /// Validate edges and compile the builder into an executable graph.
    pub fn compile(self) -> Result<Graph<S>> {
        let entry = self
            .entry_point
            .ok_or_else(|| anyhow!("entry point not set"))?;
        // 验证所有边的目标节点存在（NODE_END 除外）
        for (from, edges) in &self.edges {
            for edge in edges {
                if let Edge::Static { target } = edge {
                    if target != NODE_END && !self.nodes.contains_key(target) {
                        return Err(anyhow!(
                            "edge from '{}' points to unknown node '{}'",
                            from,
                            target
                        ));
                    }
                }
            }
        }
        Ok(Graph {
            nodes: self.nodes,
            edges: self.edges,
            entry_point: entry,
            max_iterations: self.max_iterations,
        })
    }
}

impl<S: GraphState + 'static> Default for GraphBuilder<S> {
    fn default() -> Self {
        Self::new()
    }
}

/// Chain: 顺序执行多个节点
pub struct ChainNode<S: GraphState> {
    name: String,
    nodes: Vec<Arc<dyn GraphNode<S>>>,
}

impl<S: GraphState> ChainNode<S> {
    /// Create a chain node that executes its child nodes sequentially.
    pub fn new(name: impl Into<String>, nodes: Vec<Arc<dyn GraphNode<S>>>) -> Arc<Self> {
        Arc::new(Self {
            name: name.into(),
            nodes,
        })
    }
}

#[async_trait]
impl<S: GraphState + 'static> GraphNode<S> for ChainNode<S> {
    async fn execute(&self, state: &mut S) -> Result<()> {
        for node in &self.nodes {
            node.execute(state).await?;
        }
        Ok(())
    }
    fn name(&self) -> &str {
        &self.name
    }
}

/// Parallel: 并行执行多个节点，合并结果
pub struct ParallelNode<S: GraphState + Clone + Send + 'static> {
    name: String,
    nodes: Vec<Arc<dyn GraphNode<S>>>,
}

impl<S: GraphState + Clone + Send + 'static> ParallelNode<S> {
    /// Create a parallel node that executes its children concurrently and merges results.
    pub fn new(name: impl Into<String>, nodes: Vec<Arc<dyn GraphNode<S>>>) -> Arc<Self> {
        Arc::new(Self {
            name: name.into(),
            nodes,
        })
    }
}

#[async_trait]
impl<S: GraphState + Clone + Send + 'static> GraphNode<S> for ParallelNode<S> {
    async fn execute(&self, state: &mut S) -> Result<()> {
        let mut handles = vec![];
        for node in &self.nodes {
            let mut s = state.clone();
            let node = node.clone();
            handles.push(tokio::spawn(async move {
                node.execute(&mut s).await?;
                Ok::<S, anyhow::Error>(s)
            }));
        }
        for handle in handles {
            let result_state = handle.await??;
            state.merge(result_state);
        }
        Ok(())
    }
    fn name(&self) -> &str {
        &self.name
    }
}

// ============================================================
// Channel-Based State Management
// ============================================================

/// Snapshot of a channel for checkpointing
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelCheckpoint {
    pub kind: String,
    pub data: Value,
}

/// Core Channel trait: each state field is an independent channel with its own update semantics
pub trait Channel: Send + Sync {
    type Value: Clone + Send + Sync;
    type Update: Send;

    fn get(&self) -> Option<&Self::Value>;
    /// Apply updates; returns true if the value actually changed
    fn update(&mut self, updates: Vec<Self::Update>) -> bool;
    fn checkpoint(&self) -> ChannelCheckpoint;
    /// Mark this channel as consumed (prevents re-triggering)
    fn consume(&mut self) -> bool;
    fn is_available(&self) -> bool;
}

/// LastValue channel: last writer wins
pub struct LastValue<T> {
    pub value: Option<T>,
    pub consumed: bool,
}

impl<T: Clone + Send + Sync> LastValue<T> {
    /// Create an empty last-value channel.
    pub fn new() -> Self {
        Self { value: None, consumed: false }
    }
    /// Create a last-value channel initialized with a value.
    pub fn with_value(v: T) -> Self {
        Self { value: Some(v), consumed: false }
    }
}

impl<T: Clone + Send + Sync + Default> Default for LastValue<T> {
    fn default() -> Self { Self::new() }
}

impl<T: Clone + Send + Sync + Serialize + for<'de> Deserialize<'de> + 'static> Channel for LastValue<T> {
    type Value = T;
    type Update = T;

    fn get(&self) -> Option<&T> { self.value.as_ref() }

    fn update(&mut self, updates: Vec<T>) -> bool {
        if let Some(last) = updates.into_iter().last() {
            self.value = Some(last);
            self.consumed = false;
            true
        } else {
            false
        }
    }

    fn checkpoint(&self) -> ChannelCheckpoint {
        ChannelCheckpoint {
            kind: "LastValue".to_string(),
            data: serde_json::to_value(&self.value).unwrap_or(Value::Null),
        }
    }

    fn consume(&mut self) -> bool {
        if !self.consumed && self.value.is_some() {
            self.consumed = true;
            true
        } else {
            false
        }
    }

    fn is_available(&self) -> bool {
        self.value.is_some() && !self.consumed
    }
}

/// Aggregate channel: combines multiple writes using a reducer function
pub struct Aggregate<T, F> {
    pub value: T,
    reducer: F,
    dirty: bool,
    consumed: bool,
}

impl<T, F> Aggregate<T, F>
where
    T: Clone + Send + Sync,
    F: Fn(T, T) -> T + Send + Sync,
{
    /// Create an aggregate channel with an initial value and a reducer function.
    pub fn new(init: T, reducer: F) -> Self {
        Self { value: init, reducer, dirty: false, consumed: false }
    }
}

impl<T, F> Channel for Aggregate<T, F>
where
    T: Clone + Send + Sync + Serialize + for<'de> Deserialize<'de> + 'static,
    F: Fn(T, T) -> T + Send + Sync + 'static,
{
    type Value = T;
    type Update = T;

    fn get(&self) -> Option<&T> { Some(&self.value) }

    fn update(&mut self, updates: Vec<T>) -> bool {
        if updates.is_empty() { return false; }
        for u in updates {
            // Safety: we need to move self.value out temporarily.
            // We use a dummy clone only as placeholder, then overwrite.
            let acc = self.value.clone();
            self.value = (self.reducer)(acc, u);
        }
        self.dirty = true;
        self.consumed = false;
        true
    }

    fn checkpoint(&self) -> ChannelCheckpoint {
        ChannelCheckpoint {
            kind: "Aggregate".to_string(),
            data: serde_json::to_value(&self.value).unwrap_or(Value::Null),
        }
    }

    fn consume(&mut self) -> bool {
        if !self.consumed && self.dirty {
            self.consumed = true;
            true
        } else {
            false
        }
    }

    fn is_available(&self) -> bool { self.dirty && !self.consumed }
}

/// Topic channel: publish/subscribe queue
pub struct Topic<T> {
    pub items: Vec<T>,
    consumed: bool,
}

impl<T: Clone + Send + Sync> Topic<T> {
    /// Create an empty topic channel.
    pub fn new() -> Self { Self { items: vec![], consumed: false } }
}

impl<T: Clone + Send + Sync> Default for Topic<T> {
    fn default() -> Self { Self::new() }
}

impl<T: Clone + Send + Sync + Serialize + for<'de> Deserialize<'de> + 'static> Channel for Topic<T> {
    type Value = Vec<T>;
    type Update = T;

    fn get(&self) -> Option<&Vec<T>> { if self.items.is_empty() { None } else { Some(&self.items) } }

    fn update(&mut self, updates: Vec<T>) -> bool {
        if updates.is_empty() { return false; }
        self.items.extend(updates);
        self.consumed = false;
        true
    }

    fn checkpoint(&self) -> ChannelCheckpoint {
        ChannelCheckpoint {
            kind: "Topic".to_string(),
            data: serde_json::to_value(&self.items).unwrap_or(Value::Null),
        }
    }

    fn consume(&mut self) -> bool {
        if !self.consumed && !self.items.is_empty() {
            self.consumed = true;
            true
        } else {
            false
        }
    }

    fn is_available(&self) -> bool { !self.items.is_empty() && !self.consumed }
}

/// Barrier channel: waits for all named sources to write before becoming available
pub struct Barrier {
    required: std::collections::HashSet<String>,
    received: std::collections::HashSet<String>,
    pub value: Option<Value>,
    consumed: bool,
}

impl Barrier {
    /// Create a barrier that waits for writes from all named sources.
    pub fn new(required: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            required: required.into_iter().map(|s| s.into()).collect(),
            received: std::collections::HashSet::new(),
            value: None,
            consumed: false,
        }
    }

    /// Record a write from the named source with the given value.
    pub fn write_from(&mut self, source: impl Into<String>, val: Value) {
        self.received.insert(source.into());
        self.value = Some(val);
        self.consumed = false;
    }

    /// Returns true if all required sources have written.
    pub fn is_complete(&self) -> bool {
        self.required.iter().all(|r| self.received.contains(r))
    }

    /// Consume the barrier if complete. Returns true on first consume.
    pub fn consume_barrier(&mut self) -> bool {
        if self.is_complete() && !self.consumed {
            self.consumed = true;
            true
        } else {
            false
        }
    }
}

/// Ephemeral channel: read-once (cleared after consume)
pub struct Ephemeral<T> {
    pub value: Option<T>,
}

impl<T: Clone + Send + Sync> Ephemeral<T> {
    /// Create an empty ephemeral channel.
    pub fn new() -> Self { Self { value: None } }
    /// Set the ephemeral value.
    pub fn set(&mut self, v: T) { self.value = Some(v); }
    /// Take the value, leaving the channel empty.
    pub fn take(&mut self) -> Option<T> { self.value.take() }
}

impl<T: Clone + Send + Sync> Default for Ephemeral<T> {
    fn default() -> Self { Self::new() }
}

// ============================================================
// Checkpoint / Durability
// ============================================================

/// Full graph execution snapshot
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub id: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    /// Serialized channel values (channel_name -> checkpoint)
    pub channel_values: HashMap<String, ChannelCheckpoint>,
    /// Channel monotonic versions
    pub channel_versions: HashMap<String, u64>,
    /// Per-node: which channel versions have been seen
    pub versions_seen: HashMap<String, HashMap<String, u64>>,
    /// Pending interrupt resume values (idx -> value)
    pub interrupt_resume: Vec<Value>,
}

impl Checkpoint {
    /// Create a new empty checkpoint with a unique ID.
    pub fn new() -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: chrono::Utc::now(),
            channel_values: HashMap::new(),
            channel_versions: HashMap::new(),
            versions_seen: HashMap::new(),
            interrupt_resume: vec![],
        }
    }
}

impl Default for Checkpoint {
    fn default() -> Self { Self::new() }
}

/// Summary info for listing checkpoints
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointMeta {
    pub id: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

/// Durability mode for checkpoint saving
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurabilityMode {
    /// Save checkpoint before each step (safe, slower)
    Sync,
    /// Save checkpoint asynchronously (fast, may lose last step)
    Async,
    /// Save only when graph finishes
    Exit,
}

/// Trait for persisting and restoring checkpoints
#[async_trait]
pub trait CheckpointSaver: Send + Sync {
    async fn put(&self, checkpoint: &Checkpoint) -> Result<()>;
    async fn get(&self, id: &str) -> Result<Option<Checkpoint>>;
    async fn list(&self, limit: usize) -> Result<Vec<CheckpointMeta>>;
    async fn latest(&self) -> Result<Option<Checkpoint>>;
}

/// In-memory checkpoint saver (useful for testing)
pub struct InMemoryCheckpointSaver {
    inner: tokio::sync::Mutex<Vec<Checkpoint>>,
}

impl InMemoryCheckpointSaver {
    /// Create a new in-memory checkpoint saver wrapped in an Arc.
    pub fn new() -> Arc<Self> {
        Arc::new(Self { inner: tokio::sync::Mutex::new(vec![]) })
    }
}

impl Default for InMemoryCheckpointSaver {
    fn default() -> Self {
        Self { inner: tokio::sync::Mutex::new(vec![]) }
    }
}

#[async_trait]
impl CheckpointSaver for InMemoryCheckpointSaver {
    async fn put(&self, checkpoint: &Checkpoint) -> Result<()> {
        self.inner.lock().await.push(checkpoint.clone());
        Ok(())
    }

    async fn get(&self, id: &str) -> Result<Option<Checkpoint>> {
        Ok(self.inner.lock().await.iter().find(|c| c.id == id).cloned())
    }

    async fn list(&self, limit: usize) -> Result<Vec<CheckpointMeta>> {
        let guard = self.inner.lock().await;
        Ok(guard.iter().rev().take(limit).map(|c| CheckpointMeta {
            id: c.id.clone(),
            timestamp: c.timestamp,
        }).collect())
    }

    async fn latest(&self) -> Result<Option<Checkpoint>> {
        Ok(self.inner.lock().await.last().cloned())
    }
}

// ============================================================
// Human-in-the-loop Interrupt / Resume
// ============================================================

/// Interrupt signal raised inside a node
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphInterrupt {
    pub id: String,
    pub value: Value,
    /// Which interrupt index this is (for multi-interrupt nodes)
    pub index: usize,
}

/// Execution result indicating an interrupt occurred
#[derive(Debug)]
pub enum GraphExecutionResult {
    /// Graph completed normally
    Done,
    /// Graph was interrupted and needs human input
    Interrupted { interrupt: GraphInterrupt, checkpoint_id: String },
}

/// Command to resume an interrupted graph
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeCommand {
    /// The resume value provided by the human
    pub resume: Value,
    /// Optional: override which node to jump to
    pub goto: Option<String>,
    /// Optional: state update to apply before resuming
    pub update: Option<Value>,
}

/// Per-execution scratchpad for interrupt tracking
#[derive(Debug, Default, Clone)]
pub struct InterruptScratchpad {
    pub interrupt_counter: usize,
    pub resume_values: Vec<Value>,
}

impl InterruptScratchpad {
    /// Create a scratchpad with the given resume values for human-in-the-loop.
    pub fn new(resume_values: Vec<Value>) -> Self {
        Self { interrupt_counter: 0, resume_values }
    }

    /// Call this from inside a node to pause for human input.
    /// If a resume value is available, returns it. Otherwise returns Err with interrupt info.
    pub fn interrupt(&mut self, value: impl Serialize) -> Result<Value> {
        let idx = self.interrupt_counter;
        self.interrupt_counter += 1;

        if let Some(resume_val) = self.resume_values.get(idx).cloned() {
            return Ok(resume_val);
        }

        let serialized = serde_json::to_value(&value)
            .unwrap_or(Value::String("interrupt".to_string()));
        Err(anyhow::Error::new(InterruptError {
            interrupt: GraphInterrupt {
                id: uuid::Uuid::new_v4().to_string(),
                value: serialized,
                index: idx,
            },
        }))
    }
}

/// Error type carrying interrupt info (used internally)
#[derive(Debug, thiserror::Error)]
#[error("Graph interrupted at index {}: {}", interrupt.index, interrupt.id)]
pub struct InterruptError {
    pub interrupt: GraphInterrupt,
}

// ============================================================
// Version Triggering (BSP-style)
// ============================================================

/// BSP-style execution result
#[derive(Debug)]
pub enum TickResult {
    /// More steps needed
    Continue,
    /// Execution complete
    Done,
    /// Interrupted, needs human input
    Interrupted(GraphInterrupt),
}

/// Channel write produced by a node
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelWrite {
    pub channel: String,
    pub value: Value,
}

/// Task status for idempotent retry
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskStatus {
    Pending,
    Completed,
    Failed,
    Interrupted,
}

/// Task record for idempotent execution
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    pub node_name: String,
    pub writes: Vec<ChannelWrite>,
    pub status: TaskStatus,
}

/// Dynamic fan-out target
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendTarget {
    /// Target node name
    pub node: String,
    /// Argument to pass (merges into state)
    pub arg: Value,
}

// ============================================================
// Enhanced Graph with Checkpoint + Interrupt support
// ============================================================

/// Graph execution options
pub struct GraphRunConfig {
    pub checkpoint_saver: Option<Arc<dyn CheckpointSaver>>,
    pub durability: DurabilityMode,
    pub resume_command: Option<ResumeCommand>,
    /// Node names where execution should pause BEFORE running
    pub interrupt_before: Vec<String>,
    /// Node names where execution should pause AFTER running
    pub interrupt_after: Vec<String>,
    /// Max BSP supersteps (recursion limit)
    pub recursion_limit: usize,
}

impl Default for GraphRunConfig {
    fn default() -> Self {
        Self {
            checkpoint_saver: None,
            durability: DurabilityMode::Exit,
            resume_command: None,
            interrupt_before: vec![],
            interrupt_after: vec![],
            recursion_limit: 25,
        }
    }
}

impl<S: GraphState + 'static> Graph<S> {
    /// Execute the graph with advanced config (checkpoint, interrupt, resume)
    pub async fn execute_with_config(
        &self,
        state: &mut S,
        config: &GraphRunConfig,
    ) -> Result<GraphExecutionResult> {
        let resume_values = config.resume_command.as_ref()
            .map(|c| vec![c.resume.clone()])
            .unwrap_or_default();
        let mut scratchpad = InterruptScratchpad::new(resume_values);

        let mut current = config.resume_command.as_ref()
            .and_then(|c| c.goto.clone())
            .unwrap_or_else(|| self.entry_point.clone());
        let mut iterations = 0;

        loop {
            if current == NODE_END || iterations >= config.recursion_limit {
                break;
            }
            iterations += 1;

            // Check interrupt_before
            if config.interrupt_before.contains(&current) {
                let interrupt = GraphInterrupt {
                    id: uuid::Uuid::new_v4().to_string(),
                    value: Value::String(format!("before:{}", current)),
                    index: scratchpad.interrupt_counter,
                };
                let cp_id = if let Some(saver) = &config.checkpoint_saver {
                    let cp = Checkpoint::new();
                    saver.put(&cp).await?;
                    cp.id
                } else {
                    "no-checkpoint".to_string()
                };
                return Ok(GraphExecutionResult::Interrupted { interrupt, checkpoint_id: cp_id });
            }

            if let Some(node) = self.nodes.get(&current) {
                tracing::debug!(node = current, iteration = iterations, "executing graph node (with config)");
                match node.execute(state).await {
                    Ok(()) => {}
                    Err(e) => {
                        if let Some(ie) = e.downcast_ref::<InterruptError>() {
                            let interrupt = ie.interrupt.clone();
                            let cp_id = if let Some(saver) = &config.checkpoint_saver {
                                let cp = Checkpoint::new();
                                saver.put(&cp).await?;
                                cp.id
                            } else {
                                "no-checkpoint".to_string()
                            };
                            return Ok(GraphExecutionResult::Interrupted { interrupt, checkpoint_id: cp_id });
                        }
                        return Err(e);
                    }
                }
            } else if current != NODE_START {
                return Err(anyhow!("graph node not found: {}", current));
            }

            // Checkpoint (Sync mode: after each step)
            if config.durability == DurabilityMode::Sync {
                if let Some(saver) = &config.checkpoint_saver {
                    let cp = Checkpoint::new();
                    saver.put(&cp).await?;
                }
            }

            // Check interrupt_after
            if config.interrupt_after.contains(&current) {
                let interrupt = GraphInterrupt {
                    id: uuid::Uuid::new_v4().to_string(),
                    value: Value::String(format!("after:{}", current)),
                    index: scratchpad.interrupt_counter,
                };
                let cp_id = if let Some(saver) = &config.checkpoint_saver {
                    let cp = Checkpoint::new();
                    saver.put(&cp).await?;
                    cp.id
                } else {
                    "no-checkpoint".to_string()
                };
                return Ok(GraphExecutionResult::Interrupted { interrupt, checkpoint_id: cp_id });
            }

            let _ = &mut scratchpad; // suppress unused warning
            current = self.resolve_next(&current, state)?;
        }

        // Final checkpoint (Exit mode)
        if config.durability == DurabilityMode::Exit {
            if let Some(saver) = &config.checkpoint_saver {
                let cp = Checkpoint::new();
                saver.put(&cp).await?;
            }
        }

        if iterations >= config.recursion_limit {
            tracing::warn!("graph reached recursion_limit={}", config.recursion_limit);
        }

        Ok(GraphExecutionResult::Done)
    }
}

/// RetryPolicy for per-node retry with exponential backoff
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_attempts: usize,
    pub initial_delay_ms: u64,
    pub backoff_factor: f64,
    pub jitter: bool,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_delay_ms: 100,
            backoff_factor: 2.0,
            jitter: true,
        }
    }
}

impl RetryPolicy {
    /// Execute an async closure with retries and exponential backoff.
    pub async fn execute<F, Fut>(&self, mut f: F) -> Result<()>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<()>>,
    {
        let mut attempt = 0;
        loop {
            match f().await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    attempt += 1;
                    if attempt >= self.max_attempts {
                        return Err(e);
                    }
                    let delay = (self.initial_delay_ms as f64
                        * self.backoff_factor.powi(attempt as i32 - 1)) as u64;
                    let jitter_ms = if self.jitter {
                        (delay as f64 * 0.25 * rand_f64(attempt)) as u64
                    } else {
                        0
                    };
                    tokio::time::sleep(std::time::Duration::from_millis(delay + jitter_ms)).await;
                }
            }
        }
    }
}

/// Deterministic pseudo-random float for jitter (avoids pulling in rand crate)
fn rand_f64(seed: usize) -> f64 {
    let x = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    (x >> 33) as f64 / (u32::MAX as f64)
}

/// Async function that transforms a GraphState.
pub type StateFn<S> = Arc<
    dyn Fn(&mut S) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>>
        + Send
        + Sync,
>;

/// FunctionNode: 将闭包包装为 GraphNode
pub struct FunctionNode<S: GraphState> {
    name: String,
    f: StateFn<S>,
}

impl<S: GraphState + 'static> FunctionNode<S> {
    /// Create a function node from an async closure that transforms graph state.
    pub fn new<F, Fut>(name: impl Into<String>, f: F) -> Arc<Self>
    where
        F: Fn(&mut S) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<()>> + Send + 'static,
    {
        Arc::new(Self {
            name: name.into(),
            f: Arc::new(move |s| Box::pin(f(s)) as _),
        })
    }
}

#[async_trait]
impl<S: GraphState + 'static> GraphNode<S> for FunctionNode<S> {
    async fn execute(&self, state: &mut S) -> Result<()> {
        (self.f)(state).await
    }
    fn name(&self) -> &str {
        &self.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- Test GraphState for compile/execute tests --
    #[derive(Clone, Default, Debug)]
    struct TestState {
        log: Vec<String>,
    }

    impl GraphState for TestState {
        fn merge(&mut self, mut other: Self) {
            self.log.append(&mut other.log);
        }
    }

    // Simple node that pushes its name to state log
    struct LogNode(String);
    #[async_trait]
    impl GraphNode<TestState> for LogNode {
        async fn execute(&self, state: &mut TestState) -> Result<()> {
            state.log.push(self.0.clone());
            Ok(())
        }
        fn name(&self) -> &str { &self.0 }
    }

    #[test]
    fn test_graph_builder_compile_valid() {
        let graph = GraphBuilder::<TestState>::new()
            .add_node("a", Arc::new(LogNode("a".into())))
            .add_node("b", Arc::new(LogNode("b".into())))
            .add_edge("a", "b")
            .add_edge("b", NODE_END)
            .set_entry("a")
            .compile();
        assert!(graph.is_ok());
    }

    #[test]
    fn test_graph_builder_compile_missing_target() {
        let graph = GraphBuilder::<TestState>::new()
            .add_node("a", Arc::new(LogNode("a".into())))
            // Edge points to "nonexistent" which is not registered
            .add_edge("a", "nonexistent")
            .set_entry("a")
            .compile();
        assert!(graph.is_err());
        let err_msg = format!("{}", graph.err().unwrap());
        assert!(err_msg.contains("nonexistent"), "error: {err_msg}");
    }

    #[test]
    fn test_graph_builder_compile_no_entry() {
        let graph = GraphBuilder::<TestState>::new()
            .add_node("a", Arc::new(LogNode("a".into())))
            .compile();
        assert!(graph.is_err());
        assert!(format!("{}", graph.err().unwrap()).contains("entry point"));
    }

    #[tokio::test]
    async fn test_graph_execute_simple_chain() {
        let graph = GraphBuilder::<TestState>::new()
            .add_node("a", Arc::new(LogNode("a".into())))
            .add_node("b", Arc::new(LogNode("b".into())))
            .add_edge("a", "b")
            .add_edge("b", NODE_END)
            .set_entry("a")
            .compile()
            .unwrap();

        let mut state = TestState::default();
        graph.execute(&mut state).await.unwrap();
        assert_eq!(state.log, vec!["a", "b"]);
    }

    #[tokio::test]
    async fn test_graph_execute_conditional_edge() {
        let graph = GraphBuilder::<TestState>::new()
            .add_node("decide", Arc::new(LogNode("decide".into())))
            .add_node("path_a", Arc::new(LogNode("path_a".into())))
            .add_node("path_b", Arc::new(LogNode("path_b".into())))
            .add_conditional_edge("decide", |s: &TestState| {
                if s.log.contains(&"decide".to_string()) { "path_a".into() } else { "path_b".into() }
            })
            .add_edge("path_a", NODE_END)
            .add_edge("path_b", NODE_END)
            .set_entry("decide")
            .compile()
            .unwrap();

        let mut state = TestState::default();
        graph.execute(&mut state).await.unwrap();
        // "decide" was executed, so condition routes to path_a
        assert_eq!(state.log, vec!["decide", "path_a"]);
    }

    // -- LastValue channel tests --

    #[test]
    fn test_last_value_update_consume() {
        let mut lv = LastValue::<i32>::new();
        assert!(lv.get().is_none());
        assert!(!lv.is_available());

        let changed = lv.update(vec![42]);
        assert!(changed);
        assert_eq!(lv.get(), Some(&42));
        assert!(lv.is_available());

        // Consume
        assert!(lv.consume());
        assert!(!lv.is_available());
        // Double consume returns false
        assert!(!lv.consume());
    }

    #[test]
    fn test_last_value_last_writer_wins() {
        let mut lv = LastValue::<i32>::new();
        lv.update(vec![1, 2, 3]);
        assert_eq!(lv.get(), Some(&3));
    }

    #[test]
    fn test_last_value_update_empty_vec() {
        let mut lv = LastValue::<i32>::new();
        let changed = lv.update(vec![]);
        assert!(!changed);
    }

    #[test]
    fn test_last_value_checkpoint() {
        let lv = LastValue::with_value(99);
        let ckpt = lv.checkpoint();
        assert_eq!(ckpt.kind, "LastValue");
        assert_eq!(ckpt.data, serde_json::json!(99));
    }

    // -- Aggregate channel tests --

    #[test]
    fn test_aggregate_reducer_sum() {
        let mut agg = Aggregate::new(0, |acc, val| acc + val);
        assert_eq!(agg.get(), Some(&0));

        agg.update(vec![10, 20, 30]);
        assert_eq!(agg.get(), Some(&60));
        assert!(agg.is_available());
    }

    #[test]
    fn test_aggregate_consume() {
        let mut agg = Aggregate::new(0, |acc, val| acc + val);
        agg.update(vec![5]);
        assert!(agg.consume());
        assert!(!agg.is_available());
        assert!(!agg.consume()); // double consume
    }

    #[test]
    fn test_aggregate_update_empty() {
        let mut agg = Aggregate::new(0, |acc, val| acc + val);
        let changed = agg.update(vec![]);
        assert!(!changed);
    }

    #[test]
    fn test_aggregate_checkpoint() {
        let mut agg = Aggregate::new(0, |acc, val| acc + val);
        agg.update(vec![1, 2]);
        let ckpt = agg.checkpoint();
        assert_eq!(ckpt.kind, "Aggregate");
        assert_eq!(ckpt.data, serde_json::json!(3));
    }
}
