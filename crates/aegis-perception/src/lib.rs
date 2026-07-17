//! # aegis-perception
//!
//! Ambient perception triggers for the Aegis agent.
//!
//! Enables environment-aware activation without explicit user input:
//! - **Cron**: scheduled triggers (e.g., daily briefing at 09:00)
//! - **Webhook**: HTTP endpoint for external system integration
//! - **Metrics**: Prometheus-compatible metrics collection

pub mod cron;
pub mod webhook;
pub use cron::CronTrigger;
pub mod metrics;
pub use webhook::WebhookServer;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// Event priority for filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Priority {
    Low = 0,
    Medium = 1,
    High = 2,
    Critical = 3,
}

/// Unified event type for the perception system.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: String,
    pub source: EventSource,
    pub priority: Priority,
    pub timestamp: DateTime<Utc>,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventSource {
    Cron { expression: String },
    FileWatch { path: String },
    Webhook { endpoint: String },
    System { kind: String },
    User { channel: String },
}

impl Event {
    /// Creates a new `instance`.
    pub fn new(source: EventSource, priority: Priority, payload: serde_json::Value) -> Self {
        Self {
            id: uuid_short(),
            source,
            priority,
            timestamp: Utc::now(),
            payload,
        }
    }
}

fn uuid_short() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("evt-{:x}{:04x}", t.as_secs(), t.subsec_millis())
}

/// The event bus: broadcast channel for distributing events.
pub struct EventBus {
    tx: broadcast::Sender<Event>,
}

impl EventBus {
    /// Creates a new `instance`.
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    /// Publish an event to all subscribers.
    pub fn publish(&self, event: Event) -> usize {
        self.tx.send(event).unwrap_or(0)
    }

    /// Subscribe to events.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    /// Convenience: publish a system event.
    pub fn system_event(&self, kind: &str, payload: serde_json::Value) {
        self.publish(Event::new(
            EventSource::System {
                kind: kind.to_string(),
            },
            Priority::Medium,
            payload,
        ));
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(256)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_new() {
        let source = EventSource::System {
            kind: "test".into(),
        };
        let payload = serde_json::json!({"key": "value"});
        let event = Event::new(source.clone(), Priority::High, payload.clone());
        assert!(
            event.id.starts_with("evt-"),
            "id should start with evt-, got: {}",
            event.id
        );
        assert_eq!(event.priority, Priority::High);
        assert_eq!(event.payload, payload);
    }

    #[test]
    fn test_uuid_short_format() {
        let id = uuid_short();
        assert!(
            id.starts_with("evt-"),
            "uuid_short should start with 'evt-': {id}"
        );
        // evt- prefix + hex timestamp + 4 hex digits for millis
        assert!(id.len() > 4);
    }

    #[test]
    fn test_priority_ordering() {
        assert!(Priority::Low < Priority::Medium);
        assert!(Priority::Medium < Priority::High);
        assert!(Priority::High < Priority::Critical);
    }

    #[test]
    fn test_priority_equality() {
        assert_eq!(Priority::Low, Priority::Low);
        assert_ne!(Priority::Low, Priority::High);
    }

    #[test]
    fn test_event_bus_publish_subscribe() {
        let bus = EventBus::new(16);
        let mut rx = bus.subscribe();

        let event = Event::new(
            EventSource::System {
                kind: "ping".into(),
            },
            Priority::Medium,
            serde_json::json!({"msg": "hello"}),
        );
        let count = bus.publish(event);
        assert_eq!(count, 1);

        let received = rx.try_recv().unwrap();
        assert_eq!(received.priority, Priority::Medium);
        assert_eq!(received.payload, serde_json::json!({"msg": "hello"}));
    }

    #[test]
    fn test_event_bus_multiple_subscribers() {
        let bus = EventBus::new(16);
        let mut rx1 = bus.subscribe();
        let mut rx2 = bus.subscribe();

        let event = Event::new(
            EventSource::User {
                channel: "slack".into(),
            },
            Priority::Low,
            serde_json::json!(null),
        );
        let count = bus.publish(event);
        assert_eq!(count, 2);

        assert!(rx1.try_recv().is_ok());
        assert!(rx2.try_recv().is_ok());
    }

    #[test]
    fn test_event_bus_system_event_convenience() {
        let bus = EventBus::new(16);
        let mut rx = bus.subscribe();
        bus.system_event("heartbeat", serde_json::json!({"ok": true}));
        let event = rx.try_recv().unwrap();
        assert_eq!(event.priority, Priority::Medium);
        match &event.source {
            EventSource::System { kind } => assert_eq!(kind, "heartbeat"),
            _ => panic!("expected System source"),
        }
    }

    #[test]
    fn test_event_bus_default_capacity() {
        let bus = EventBus::default();
        let mut rx = bus.subscribe();
        bus.publish(Event::new(
            EventSource::Cron {
                expression: "* * * * *".into(),
            },
            Priority::Critical,
            serde_json::json!(null),
        ));
        assert!(rx.try_recv().is_ok());
    }

    #[test]
    fn test_event_source_filewatch() {
        let source = EventSource::FileWatch {
            path: "/tmp/test.txt".into(),
        };
        let event = Event::new(source, Priority::Low, serde_json::json!({"changed": true}));
        match &event.source {
            EventSource::FileWatch { path } => assert_eq!(path, "/tmp/test.txt"),
            _ => panic!("expected FileWatch"),
        }
    }

    #[test]
    fn test_event_source_webhook() {
        let source = EventSource::Webhook {
            endpoint: "/api/webhook".into(),
        };
        let event = Event::new(source, Priority::High, serde_json::json!({"data": 42}));
        match &event.source {
            EventSource::Webhook { endpoint } => assert_eq!(endpoint, "/api/webhook"),
            _ => panic!("expected Webhook"),
        }
    }

    #[test]
    fn test_event_bus_zero_subscriber_count() {
        let bus = EventBus::new(16);
        let event = Event::new(
            EventSource::System {
                kind: "ping".into(),
            },
            Priority::Medium,
            serde_json::json!(null),
        );
        let count = bus.publish(event);
        assert_eq!(count, 0);
    }
}
