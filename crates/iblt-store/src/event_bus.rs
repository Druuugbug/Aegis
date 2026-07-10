//! Event bus for distributing storage events to subscribers.
//!
//! Provides a publish-subscribe mechanism for storage events,
//! allowing monitoring, logging, and alerting systems to react
//! to store operations in real time.

use crate::event::{EventKind, StorageEvent};
use std::collections::HashMap;

/// Subscriber identifier.
pub type SubscriberId = u64;

/// Event filter for selective subscription.
#[derive(Debug, Clone, Default)]
pub struct EventFilter {
    /// Only receive events matching these kinds (empty = all).
    pub kinds: Vec<String>,
    /// Only receive error events.
    pub errors_only: bool,
}

impl EventFilter {
    /// Check if an event matches this filter.
    pub fn matches(&self, event: &StorageEvent) -> bool {
        if self.errors_only && !event.is_error() {
            return false;
        }
        if self.kinds.is_empty() {
            return true;
        }
        let kind_name = match &event.kind {
            EventKind::Put { .. } => "put",
            EventKind::Delete { .. } => "delete",
            EventKind::Access { .. } => "access",
            EventKind::Drain { .. } => "drain",
            EventKind::Promote { .. } => "promote",
            EventKind::Compact { .. } => "compact",
            EventKind::Checkpoint { .. } => "checkpoint",
            EventKind::Cleanup { .. } => "cleanup",
            EventKind::Error { .. } => "error",
        };
        self.kinds.iter().any(|k| k == kind_name)
    }
}

/// A subscriber callback.
#[derive(Debug)]
pub struct Subscriber {
    pub id: SubscriberId,
    pub filter: EventFilter,
    pub events: Vec<StorageEvent>,
}

/// Event bus for distributing events.
#[derive(Debug)]
pub struct EventBus {
    /// Subscribers.
    subscribers: HashMap<SubscriberId, Subscriber>,
    /// Next subscriber ID.
    next_subscriber_id: SubscriberId,
    /// Total events published.
    total_published: u64,
    /// Whether the bus is enabled.
    enabled: bool,
}

impl EventBus {
    /// Create a new event bus.
    pub fn new(enabled: bool) -> Self {
        Self {
            subscribers: HashMap::new(),
            next_subscriber_id: 1,
            total_published: 0,
            enabled,
        }
    }

    /// Subscribe to events with a filter.
    pub fn subscribe(&mut self, filter: EventFilter) -> SubscriberId {
        let id = self.next_subscriber_id;
        self.next_subscriber_id += 1;
        self.subscribers.insert(
            id,
            Subscriber {
                id,
                filter,
                events: Vec::new(),
            },
        );
        id
    }

    /// Unsubscribe a subscriber.
    pub fn unsubscribe(&mut self, id: SubscriberId) -> bool {
        self.subscribers.remove(&id).is_some()
    }

    /// Publish an event to all matching subscribers.
    pub fn publish(&mut self, event: StorageEvent) {
        if !self.enabled {
            return;
        }
        self.total_published += 1;
        for subscriber in self.subscribers.values_mut() {
            if subscriber.filter.matches(&event) {
                subscriber.events.push(event.clone());
            }
        }
    }

    /// Drain events for a subscriber.
    pub fn drain_events(&mut self, id: SubscriberId) -> Vec<StorageEvent> {
        self.subscribers
            .get_mut(&id)
            .map(|s| std::mem::take(&mut s.events))
            .unwrap_or_default()
    }

    /// Number of subscribers.
    pub fn subscriber_count(&self) -> usize {
        self.subscribers.len()
    }

    /// Total events published.
    pub fn total_published(&self) -> u64 {
        self.total_published
    }

    /// Whether the bus is enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TierLevel;

    #[test]
    fn publish_and_receive() {
        let mut bus = EventBus::new(true);
        let sub = bus.subscribe(EventFilter::default());
        bus.publish(StorageEvent::put(b"key", 10, TierLevel::Hot, 1));
        let events = bus.drain_events(sub);
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn filter_errors_only() {
        let mut bus = EventBus::new(true);
        let sub = bus.subscribe(EventFilter {
            errors_only: true,
            ..EventFilter::default()
        });
        bus.publish(StorageEvent::put(b"key", 10, TierLevel::Hot, 1));
        bus.publish(StorageEvent::error("put", "fail", 2));
        let events = bus.drain_events(sub);
        assert_eq!(events.len(), 1);
        assert!(events[0].is_error());
    }

    #[test]
    fn filter_by_kind() {
        let mut bus = EventBus::new(true);
        let sub = bus.subscribe(EventFilter {
            kinds: vec!["delete".to_string()],
            ..EventFilter::default()
        });
        bus.publish(StorageEvent::put(b"k", 1, TierLevel::Hot, 1));
        bus.publish(StorageEvent::delete(b"k", 2));
        let events = bus.drain_events(sub);
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn disabled_bus() {
        let mut bus = EventBus::new(false);
        let sub = bus.subscribe(EventFilter::default());
        bus.publish(StorageEvent::put(b"k", 1, TierLevel::Hot, 1));
        let events = bus.drain_events(sub);
        assert!(events.is_empty());
    }

    #[test]
    fn unsubscribe() {
        let mut bus = EventBus::new(true);
        let sub = bus.subscribe(EventFilter::default());
        assert!(bus.unsubscribe(sub));
        assert_eq!(bus.subscriber_count(), 0);
    }
}
