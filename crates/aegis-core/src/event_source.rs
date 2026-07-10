use anyhow::{anyhow, bail, Result};
use chrono::{DateTime, Datelike, Timelike, Utc};
use std::time::{Duration, SystemTime};
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio::time::{interval, sleep, Interval};

/// Event emitted when a persistent task trigger fires.
#[derive(Debug, Clone)]
pub struct TriggerEvent {
    pub task_id: String,
    pub fired_at: String,
    pub payload: Option<String>,
}

/// Kinds of event sources supported by the persistent task system.
#[derive(Debug, Clone)]
pub enum EventSourceKind {
    /// Cron-style schedule.
    Cron { expr: String },
    /// HTTP webhook listener.
    Webhook { port: u16 },
    /// File modification watcher (poll-based).
    FileWatch { path: String },
    /// Manual trigger only.
    Manual,
}

/// Configuration parsed from a trigger string.
#[derive(Debug, Clone)]
pub struct EventSourceConfig {
    pub kind: EventSourceKind,
    pub task_id: String,
}

impl EventSourceConfig {
    /// Parse a trigger string into an event source configuration.
    ///
    /// Supported formats:
    /// - `cron:*/5 * * * *`
    /// - `webhook:8080`
    /// - `file:/tmp/watch.txt`
    /// - `manual` (or any unrecognized string)
    pub fn from_trigger(task_id: &str, trigger: &str) -> Result<Self> {
        let kind = if let Some(expr) = trigger.strip_prefix("cron:") {
            EventSourceKind::Cron {
                expr: expr.to_string(),
            }
        } else if let Some(port_str) = trigger.strip_prefix("webhook:") {
            let port = port_str
                .parse::<u16>()
                .map_err(|_| anyhow!("invalid webhook port: {}", port_str))?;
            EventSourceKind::Webhook { port }
        } else if let Some(path) = trigger.strip_prefix("file:") {
            EventSourceKind::FileWatch {
                path: path.to_string(),
            }
        } else {
            EventSourceKind::Manual
        };

        Ok(Self {
            kind,
            task_id: task_id.to_string(),
        })
    }
}

/// An active event source that can produce `TriggerEvent`s on demand.
pub struct EventSource {
    task_id: String,
    kind: EventSourceKind,
    listener: Option<TcpListener>,
    last_mtime: Option<SystemTime>,
    interval: Option<Interval>,
}

impl EventSource {
    /// Create a new event source from the given configuration.
    pub fn new(config: EventSourceConfig) -> Self {
        Self {
            task_id: config.task_id,
            kind: config.kind,
            listener: None,
            last_mtime: None,
            interval: None,
        }
    }

    /// Wait for the next trigger event.
    pub async fn next_event(&mut self) -> Result<TriggerEvent> {
        match self.kind.clone() {
            EventSourceKind::Cron { expr } => self.next_cron_event(&expr).await,
            EventSourceKind::Webhook { port } => self.next_webhook_event(port).await,
            EventSourceKind::FileWatch { path } => self.next_filewatch_event(&path).await,
            EventSourceKind::Manual => bail!("manual trigger: call mark_running() directly"),
        }
    }

    async fn next_cron_event(&mut self, expr: &str) -> Result<TriggerEvent> {
        let now = Utc::now();

        // Iterate minute-by-minute for up to one week.
        for minute_offset in 1..=10080i64 {
            let check = now + chrono::Duration::minutes(minute_offset);
            if matches_cron(&check, expr) {
                let sleep_secs = (check - now).num_seconds().max(0) as u64;
                sleep(Duration::from_secs(sleep_secs)).await;
                return Ok(TriggerEvent {
                    task_id: self.task_id.clone(),
                    fired_at: Utc::now().to_rfc3339(),
                    payload: None,
                });
            }
        }

        bail!("no cron fire time found within the next week")
    }

    async fn next_webhook_event(&mut self, port: u16) -> Result<TriggerEvent> {
        if self.listener.is_none() {
            let listener = TcpListener::bind(("0.0.0.0", port)).await?;
            self.listener = Some(listener);
        }

        let listener = self.listener.as_ref()
            .expect("listener initialized in guard above");
        let (mut stream, _) = listener.accept().await?;

        let mut buf = vec![0u8; 4096];
        let n = stream.read(&mut buf).await.unwrap_or(0);
        buf.truncate(n);

        let raw = String::from_utf8_lossy(&buf);
        let payload = raw
            .split_once("\r\n\r\n")
            .and_then(|(_, body)| {
                if body.is_empty() {
                    None
                } else {
                    Some(body.to_string())
                }
            });

        Ok(TriggerEvent {
            task_id: self.task_id.clone(),
            fired_at: Utc::now().to_rfc3339(),
            payload,
        })
    }

    async fn next_filewatch_event(&mut self, path: &str) -> Result<TriggerEvent> {
        if self.interval.is_none() {
            self.interval = Some(interval(Duration::from_secs(5)));
            if let Ok(meta) = std::fs::metadata(path) {
                self.last_mtime = meta.modified().ok();
            }
        }

        let interval = self.interval.as_mut()
            .expect("interval initialized in guard above");

        loop {
            interval.tick().await;

            let current_mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();

            if current_mtime != self.last_mtime {
                self.last_mtime = current_mtime;
                return Ok(TriggerEvent {
                    task_id: self.task_id.clone(),
                    fired_at: Utc::now().to_rfc3339(),
                    payload: None,
                });
            }
        }
    }
}

/// Simple cron matcher supporting `*`, `*/N`, and exact numbers.
fn matches_cron(dt: &DateTime<Utc>, expr: &str) -> bool {
    let parts: Vec<&str> = expr.split_whitespace().collect();
    if parts.len() != 5 {
        return false;
    }

    let minute = dt.minute();
    let hour = dt.hour();
    let day = dt.day();
    let month = dt.month();
    let weekday = dt.weekday().num_days_from_sunday(); // 0=Sunday, 1=Monday, ...

    let min_match = match_field(parts[0], minute);
    let hour_match = match_field(parts[1], hour);
    let day_match = match_field(parts[2], day);
    let month_match = match_field(parts[3], month);
    let weekday_match = match_field(parts[4], weekday);

    // Cron standard: if both day-of-month and weekday are restricted (not *),
    // they are ORed; otherwise AND with the restricted one.
    let date_match = if parts[2] == "*" && parts[4] == "*" {
        true
    } else if parts[2] == "*" {
        weekday_match
    } else if parts[4] == "*" {
        day_match
    } else {
        day_match || weekday_match
    };

    min_match && hour_match && month_match && date_match
}

fn match_field(field: &str, value: u32) -> bool {
    if field == "*" {
        true
    } else if let Some(n) = field.strip_prefix("*/") {
        match n.parse::<u32>() {
            Ok(step) if step > 0 => value.is_multiple_of(step),
            _ => false,
        }
    } else {
        match field.parse::<u32>() {
            Ok(v) => {
                // Cron weekday: 0 and 7 both mean Sunday.
                if value == 0 && v == 7 {
                    true
                } else {
                    v == value
                }
            }
            Err(_) => false,
        }
    }
}
