use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// A simple interrupt signal that can be fired and waited on.
#[derive(Clone)]
pub struct InterruptSignal {
    flag: Arc<AtomicBool>,
    notify: Arc<tokio::sync::Notify>,
}

impl InterruptSignal {
    /// Create a new unset interrupt signal.
    pub fn new() -> Self {
        Self {
            flag: Arc::new(AtomicBool::new(false)),
            notify: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Fire the interrupt signal, notifying all waiters.
    pub fn fire(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    /// Returns true if the interrupt has been fired.
    pub fn is_set(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// Reset the interrupt flag so it can be fired again.
    pub fn reset(&self) {
        self.flag.store(false, Ordering::SeqCst);
    }

    /// Async wait until the signal is fired.
    pub async fn notified(&self) {
        self.notify.notified().await;
    }
}

impl Default for InterruptSignal {
    fn default() -> Self {
        Self::new()
    }
}

/// Source of a soft interrupt message.
#[derive(Debug, Clone)]
pub enum InterruptSource {
    User,
    System,
    SubAgent { task_id: String },
    External { channel: String },
    Goal { goal_id: String },
    Schedule { task_id: String },
}

/// A soft interrupt message to inject into the agent loop.
#[derive(Debug, Clone)]
pub struct SoftInterruptMessage {
    pub content: String,
    pub urgent: bool,
    pub source: InterruptSource,
}

/// Queue for soft interrupt messages.
#[derive(Clone, Default)]
pub struct SoftInterruptQueue {
    inner: Arc<std::sync::Mutex<Vec<SoftInterruptMessage>>>,
}

impl SoftInterruptQueue {
    /// Create an empty soft interrupt queue.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    /// Enqueue a soft interrupt message.
    pub fn push(&self, msg: SoftInterruptMessage) {
        self.inner
            .lock()
            .expect("interrupt queue lock poisoned")
            .push(msg);
    }

    /// Drain all queued messages, returning them and clearing the queue.
    pub fn drain(&self) -> Vec<SoftInterruptMessage> {
        std::mem::take(
            &mut self
                .inner
                .lock()
                .expect("interrupt queue lock poisoned"),
        )
    }

    /// Returns true if the queue has no pending messages.
    pub fn is_empty(&self) -> bool {
        self.inner
            .lock()
            .expect("interrupt queue lock poisoned")
            .is_empty()
    }
}
