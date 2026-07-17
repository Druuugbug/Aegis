//! Internal async channels for inter-component communication.
//!
//! Provides bounded and unbounded channels for communicating between
//! the hot tier, cold tier, drain controller, and event bus.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};

/// A bounded multi-producer, single-consumer channel.
#[derive(Debug)]
pub struct BoundedChannel<T> {
    inner: Arc<BoundedInner<T>>,
}

#[derive(Debug)]
struct BoundedInner<T> {
    queue: Mutex<VecDeque<T>>,
    capacity: usize,
    closed: Mutex<bool>,
    not_empty: Condvar,
    not_full: Condvar,
}

impl<T> BoundedChannel<T> {
    /// Create a new bounded channel with the given capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Arc::new(BoundedInner {
                queue: Mutex::new(VecDeque::with_capacity(capacity)),
                capacity,
                closed: Mutex::new(false),
                not_empty: Condvar::new(),
                not_full: Condvar::new(),
            }),
        }
    }

    /// Send a value. Blocks if the channel is full. Returns Err if closed.
    pub fn send(&self, value: T) -> Result<(), ChannelError> {
        let mut queue = self
            .inner
            .queue
            .lock()
            .map_err(|_| ChannelError::Poisoned)?;
        while queue.len() >= self.inner.capacity {
            let closed = self
                .inner
                .closed
                .lock()
                .map_err(|_| ChannelError::Poisoned)?;
            if *closed {
                return Err(ChannelError::Closed);
            }
            queue = self
                .inner
                .not_full
                .wait(queue)
                .map_err(|_| ChannelError::Poisoned)?;
        }
        queue.push_back(value);
        self.inner.not_empty.notify_one();
        Ok(())
    }

    /// Try to send a value without blocking. Returns Err if full or closed.
    pub fn try_send(&self, value: T) -> Result<(), ChannelError> {
        let mut queue = self
            .inner
            .queue
            .lock()
            .map_err(|_| ChannelError::Poisoned)?;
        let closed = self
            .inner
            .closed
            .lock()
            .map_err(|_| ChannelError::Poisoned)?;
        if *closed {
            return Err(ChannelError::Closed);
        }
        if queue.len() >= self.inner.capacity {
            return Err(ChannelError::Full);
        }
        queue.push_back(value);
        self.inner.not_empty.notify_one();
        Ok(())
    }

    /// Receive a value. Blocks if the channel is empty. Returns Err if closed.
    pub fn recv(&self) -> Result<T, ChannelError> {
        let mut queue = self
            .inner
            .queue
            .lock()
            .map_err(|_| ChannelError::Poisoned)?;
        loop {
            if let Some(value) = queue.pop_front() {
                self.inner.not_full.notify_one();
                return Ok(value);
            }
            let closed = self
                .inner
                .closed
                .lock()
                .map_err(|_| ChannelError::Poisoned)?;
            if *closed && queue.is_empty() {
                return Err(ChannelError::Closed);
            }
            queue = self
                .inner
                .not_empty
                .wait(queue)
                .map_err(|_| ChannelError::Poisoned)?;
        }
    }

    /// Try to receive without blocking.
    pub fn try_recv(&self) -> Result<T, ChannelError> {
        let mut queue = self
            .inner
            .queue
            .lock()
            .map_err(|_| ChannelError::Poisoned)?;
        if let Some(value) = queue.pop_front() {
            self.inner.not_full.notify_one();
            Ok(value)
        } else {
            let closed = self
                .inner
                .closed
                .lock()
                .map_err(|_| ChannelError::Poisoned)?;
            if *closed {
                Err(ChannelError::Closed)
            } else {
                Err(ChannelError::Empty)
            }
        }
    }

    /// Close the channel.
    pub fn close(&self) {
        if let Ok(mut closed) = self.inner.closed.lock() {
            *closed = true;
        }
        self.inner.not_empty.notify_all();
        self.inner.not_full.notify_all();
    }

    /// Current queue length.
    pub fn len(&self) -> usize {
        self.inner.queue.lock().map(|q| q.len()).unwrap_or(0)
    }

    /// Whether the channel is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<T> Clone for BoundedChannel<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

/// Channel errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelError {
    /// Channel is closed.
    Closed,
    /// Channel buffer is full (try_send only).
    Full,
    /// Channel buffer is empty (try_recv only).
    Empty,
    /// Internal mutex poisoned.
    Poisoned,
}

impl std::fmt::Display for ChannelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChannelError::Closed => write!(f, "channel closed"),
            ChannelError::Full => write!(f, "channel full"),
            ChannelError::Empty => write!(f, "channel empty"),
            ChannelError::Poisoned => write!(f, "channel mutex poisoned"),
        }
    }
}

impl std::error::Error for ChannelError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn send_recv_basic() {
        let ch = BoundedChannel::new(10);
        ch.send(42u32).unwrap();
        assert_eq!(ch.recv().unwrap(), 42);
    }

    #[test]
    fn try_send_full() {
        let ch = BoundedChannel::new(1);
        ch.send(1u32).unwrap();
        assert_eq!(ch.try_send(2u32), Err(ChannelError::Full));
    }

    #[test]
    fn try_recv_empty() {
        let ch = BoundedChannel::<u32>::new(1);
        assert_eq!(ch.try_recv(), Err(ChannelError::Empty));
    }

    #[test]
    fn close_drains() {
        let ch = BoundedChannel::new(10);
        ch.send(1u32).unwrap();
        ch.send(2u32).unwrap();
        ch.close();
        assert_eq!(ch.recv().unwrap(), 1);
        assert_eq!(ch.recv().unwrap(), 2);
        assert!(ch.recv().is_err());
    }

    #[test]
    fn clone_senders() {
        let ch = BoundedChannel::new(10);
        let ch2 = ch.clone();
        ch.send(1u32).unwrap();
        ch2.send(2u32).unwrap();
        assert_eq!(ch.len(), 2);
    }

    #[test]
    fn cross_thread() {
        let ch = BoundedChannel::new(10);
        let ch2 = ch.clone();
        thread::spawn(move || {
            ch2.send(99u32).unwrap();
        });
        assert_eq!(ch.recv().unwrap(), 99);
    }
}
