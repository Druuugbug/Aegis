use async_trait::async_trait;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tokio_stream::Stream;

use crate::types::*;

pub type BoxStream<T> = Pin<Box<dyn Stream<Item = T> + Send>>;

#[async_trait]
pub trait TaskManager: Send + Sync {
    async fn on_send(&self, params: TaskSendParams) -> anyhow::Result<Task>;
    async fn on_get(&self, params: TaskGetParams) -> anyhow::Result<Task>;
    async fn on_cancel(&self, params: TaskCancelParams) -> anyhow::Result<Task>;
    async fn on_subscribe(&self, params: TaskSendParams) -> anyhow::Result<BoxStream<TaskEvent>>;
    async fn publish_event(&self, task_id: &str, event: TaskEvent) -> anyhow::Result<()>;
}

type Subscribers = HashMap<String, Vec<mpsc::Sender<TaskEvent>>>;

pub struct InMemoryTaskManager {
    tasks: Arc<RwLock<HashMap<String, Task>>>,
    subscribers: Arc<RwLock<Subscribers>>,
}

impl Default for InMemoryTaskManager {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryTaskManager {
    /// Creates a new `instance`.
    pub fn new() -> Self {
        Self {
            tasks: Arc::new(RwLock::new(HashMap::new())),
            subscribers: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

#[async_trait]
impl TaskManager for InMemoryTaskManager {
    async fn on_send(&self, params: TaskSendParams) -> anyhow::Result<Task> {
        let task_id = params
            .id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let now = chrono::Utc::now();

        // A2A `message/send` carries a single `message`; accept that as well as
        // the legacy `messages` array.
        let incoming: Vec<Message> = if params.messages.is_empty() {
            params.message.clone().into_iter().collect()
        } else {
            params.messages.clone()
        };

        let mut tasks = self.tasks.write().await;

        if let Some(existing) = tasks.get_mut(&task_id) {
            // Append messages and update
            for msg in incoming {
                existing.messages.push(msg);
            }
            existing.updated_at = now;
            return Ok(existing.clone());
        }

        let task = Task {
            id: task_id.clone(),
            context_id: None,
            status: TaskStatusInfo {
                state: TaskState::Submitted,
                message: None,
                timestamp: now,
            },
            messages: incoming,
            artifacts: Vec::new(),
            kind: "task".to_string(),
            metadata: params.metadata,
            created_at: now,
            updated_at: now,
        };

        tasks.insert(task_id, task.clone());
        Ok(task)
    }

    async fn on_get(&self, params: TaskGetParams) -> anyhow::Result<Task> {
        let tasks = self.tasks.read().await;
        tasks
            .get(&params.id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Task not found: {}", params.id))
    }

    async fn on_cancel(&self, params: TaskCancelParams) -> anyhow::Result<Task> {
        let mut tasks = self.tasks.write().await;
        let task = tasks
            .get_mut(&params.id)
            .ok_or_else(|| anyhow::anyhow!("Task not found: {}", params.id))?;

        use crate::state_machine::validate_transition;
        use crate::types::TaskState as S;

        if !validate_transition(&task.status.state, &S::Canceled) {
            return Err(anyhow::anyhow!(
                "Cannot cancel task in state {:?}",
                task.status.state
            ));
        }

        let now = chrono::Utc::now();
        task.status = TaskStatusInfo {
            state: S::Canceled,
            message: None,
            timestamp: now,
        };
        task.updated_at = now;
        Ok(task.clone())
    }

    async fn on_subscribe(&self, params: TaskSendParams) -> anyhow::Result<BoxStream<TaskEvent>> {
        // Create or retrieve the task
        let task = self.on_send(params).await?;
        let task_id = task.id.clone();

        let (tx, rx) = mpsc::channel::<TaskEvent>(64);

        let mut subs = self.subscribers.write().await;
        subs.entry(task_id).or_default().push(tx);

        let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        Ok(Box::pin(stream))
    }

    async fn publish_event(&self, task_id: &str, event: TaskEvent) -> anyhow::Result<()> {
        // Update task state if status update
        if let TaskEvent::StatusUpdate(ref update) = event {
            let mut tasks = self.tasks.write().await;
            if let Some(task) = tasks.get_mut(task_id) {
                task.status = update.status.clone();
                task.updated_at = chrono::Utc::now();
            }
        }

        let subs = self.subscribers.read().await;
        if let Some(channels) = subs.get(task_id) {
            for tx in channels {
                let _ = tx.send(event.clone()).await;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_on_send_creates_task() {
        let mgr = InMemoryTaskManager::new();
        let params = TaskSendParams {
            id: Some("task-1".into()),
            message: None,
            messages: vec![],
            metadata: None,
            session_id: None,
        };
        let task = mgr.on_send(params).await.unwrap();
        assert_eq!(task.id, "task-1");
        assert_eq!(task.status.state, TaskState::Submitted);
    }

    #[tokio::test]
    async fn test_on_send_accepts_single_message() {
        // A2A message/send sends one `message`, not a `messages` array.
        let mgr = InMemoryTaskManager::new();
        let params = TaskSendParams {
            id: Some("task-msg".into()),
            message: Some(Message {
                role: MessageRole::User,
                parts: vec![Part::Text { text: "hi".into() }],
                kind: "message".into(),
                message_id: Some("m1".into()),
                context_id: None,
                task_id: None,
                metadata: None,
            }),
            messages: vec![],
            metadata: None,
            session_id: None,
        };
        let task = mgr.on_send(params).await.unwrap();
        assert_eq!(task.messages.len(), 1);
    }

    #[tokio::test]
    async fn test_on_get_retrieves_created_task() {
        let mgr = InMemoryTaskManager::new();
        let params = TaskSendParams {
            id: Some("task-2".into()),
            message: None,
            messages: vec![],
            metadata: None,
            session_id: None,
        };
        mgr.on_send(params).await.unwrap();

        let retrieved = mgr
            .on_get(TaskGetParams {
                id: "task-2".into(),
                history_length: None,
            })
            .await
            .unwrap();
        assert_eq!(retrieved.id, "task-2");
    }

    #[tokio::test]
    async fn test_on_get_nonexistent_fails() {
        let mgr = InMemoryTaskManager::new();
        let result = mgr
            .on_get(TaskGetParams {
                id: "nonexistent".into(),
                history_length: None,
            })
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_on_cancel_transitions_state() {
        let mgr = InMemoryTaskManager::new();
        let params = TaskSendParams {
            id: Some("task-3".into()),
            message: None,
            messages: vec![],
            metadata: None,
            session_id: None,
        };
        mgr.on_send(params).await.unwrap();

        let cancelled = mgr
            .on_cancel(TaskCancelParams {
                id: "task-3".into(),
                metadata: None,
            })
            .await
            .unwrap();
        assert_eq!(cancelled.status.state, TaskState::Canceled);
    }

    #[tokio::test]
    async fn test_double_cancel_fails() {
        let mgr = InMemoryTaskManager::new();
        let params = TaskSendParams {
            id: Some("task-4".into()),
            message: None,
            messages: vec![],
            metadata: None,
            session_id: None,
        };
        mgr.on_send(params).await.unwrap();

        // First cancel should succeed
        mgr.on_cancel(TaskCancelParams {
            id: "task-4".into(),
            metadata: None,
        })
        .await
        .unwrap();

        // Second cancel should fail (Canceled -> Canceled is not valid)
        let result = mgr
            .on_cancel(TaskCancelParams {
                id: "task-4".into(),
                metadata: None,
            })
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_cancel_nonexistent_fails() {
        let mgr = InMemoryTaskManager::new();
        let result = mgr
            .on_cancel(TaskCancelParams {
                id: "nope".into(),
                metadata: None,
            })
            .await;
        assert!(result.is_err());
    }
}
