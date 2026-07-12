use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// An inbound message from a channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboundMessage {
    pub channel: String,
    pub user_id: String,
    pub chat_id: String,
    pub text: String,
}

/// An outbound message to a channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboundMessage {
    pub chat_id: String,
    pub text: String,
    pub is_final: bool,
    /// If set, edit this existing message instead of sending a new one.
    pub edit_message_id: Option<String>,
    /// Reply to this message ID.
    pub reply_to: Option<String>,
}

impl OutboundMessage {
    /// Create a new final outbound message with the given chat ID and text.
    pub fn new(chat_id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            chat_id: chat_id.into(),
            text: text.into(),
            is_final: true,
            edit_message_id: None,
            reply_to: None,
        }
    }
}

/// Channel trait for multi-platform support (legacy interface).
#[async_trait]
pub trait Channel: Send + Sync {
    fn name(&self) -> &str;
    async fn connect(&mut self) -> Result<()>;
    async fn recv(&mut self) -> Result<InboundMessage>;
    async fn send(&self, msg: OutboundMessage) -> Result<()>;
}

// ── PlatformAdapter (multi-platform channel abstraction) ──────────────────────

/// Unified message event from any platform.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageEvent {
    pub text: String,
    pub message_type: MessageType,
    pub source: SessionSource,
    pub media_urls: Vec<String>,
    pub reply_to: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageType {
    Text,
    Photo,
    Video,
    Audio,
    Document,
    Command,
}

/// Identifies where a message came from (platform + chat + user).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSource {
    pub platform: String,
    pub chat_type: ChatType,
    pub chat_id: String,
    pub user_id: String,
    pub thread_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatType {
    Private,
    Group,
    Channel,
}

/// How sessions are isolated.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionIsolation {
    /// Each user in a group has their own session.
    PerUser,
    /// All users in a chat share one session.
    Shared,
    /// Each thread gets its own session.
    PerThread,
}

/// Build a deterministic session key from source + isolation config.
pub fn build_session_key(source: &SessionSource, isolation: SessionIsolation) -> String {
    match isolation {
        SessionIsolation::PerUser => format!(
            "{}:{}:{}:{}",
            source.platform, source.chat_id, source.user_id,
            match source.chat_type { ChatType::Private => "private", ChatType::Group => "group", ChatType::Channel => "channel" }
        ),
        SessionIsolation::Shared => format!(
            "{}:{}",
            source.platform, source.chat_id
        ),
        SessionIsolation::PerThread => format!(
            "{}:{}:{}",
            source.platform, source.chat_id,
            source.thread_id.as_deref().unwrap_or("main")
        ),
    }
}

/// Enhanced platform adapter trait.
/// Provides connect/send/typing/interrupt capabilities.
#[async_trait]
pub trait PlatformAdapter: Send + Sync {
    /// Platform identifier (e.g. "telegram", "discord", "slack").
    fn platform_name(&self) -> &str;

    /// Connect to the platform.
    async fn connect(&mut self) -> Result<()>;

    /// Disconnect gracefully.
    async fn disconnect(&mut self) -> Result<()>;

    /// Send a message to a chat.
    async fn send(&self, msg: &OutboundMessage) -> Result<Option<String>>;

    /// Send a typing indicator.
    async fn send_typing(&self, chat_id: &str) -> Result<()>;

    /// Edit an existing message (for streaming progressive updates).
    async fn edit_message(&self, chat_id: &str, message_id: &str, text: &str) -> Result<()>;

    /// Interrupt an active agent session (signal new message arrived).
    fn interrupt_session(&self, session_key: &str);

    /// Receive next incoming message event.
    async fn recv(&mut self) -> Result<MessageEvent>;
}

// ── Streaming progressive push (StreamConsumer) ───────────────────────────────

/// Bridges Agent stream output → Platform progressive message edits.
/// Implements flood control: backs off on edit failures, degrades after 3 failures.
pub struct StreamConsumer {
    /// Accumulated text buffer.
    pub buffer: String,
    last_edit: std::time::Instant,
    min_edit_interval: std::time::Duration,
    flood_backoff: std::time::Duration,
    consecutive_failures: u32,
    /// After 3 failures, streaming is disabled (fall back to final send).
    pub streaming_disabled: bool,
}

impl StreamConsumer {
    /// Create a new stream consumer with the minimum edit interval in milliseconds.
    pub fn new(min_edit_interval_ms: u64) -> Self {
        Self {
            buffer: String::new(),
            last_edit: std::time::Instant::now(),
            min_edit_interval: std::time::Duration::from_millis(min_edit_interval_ms),
            flood_backoff: std::time::Duration::from_secs(1),
            consecutive_failures: 0,
            streaming_disabled: false,
        }
    }

    /// Append delta text. Returns true if an edit should be flushed now.
    pub fn push_delta(&mut self, text: &str) -> bool {
        self.buffer.push_str(text);
        if self.streaming_disabled {
            return false;
        }
        self.last_edit.elapsed() >= self.min_edit_interval
    }

    /// Record a successful edit.
    pub fn on_edit_success(&mut self) {
        self.last_edit = std::time::Instant::now();
        self.flood_backoff = std::time::Duration::from_secs(1);
        self.consecutive_failures = 0;
    }

    /// Record a failed edit. Returns whether streaming is still enabled.
    pub fn on_edit_failure(&mut self) -> bool {
        self.consecutive_failures += 1;
        self.flood_backoff *= 2;
        if self.consecutive_failures >= 3 {
            self.streaming_disabled = true;
            return false;
        }
        true
    }

    /// Effective interval to wait before next edit (includes backoff).
    pub fn effective_interval(&self) -> std::time::Duration {
        self.min_edit_interval.max(self.flood_backoff)
    }
}

// ── File-based external intervention ──────────────────────────────────────────

/// Watches files for external process injection.
///
/// External process writes to `intervene_path` → Agent injects as user message next turn.
/// External process writes to `keyinfo_path` → Agent updates key_info context.
pub struct FileInterventionWatcher {
    /// Path to watch for injected messages.
    pub intervene_path: std::path::PathBuf,
    /// Path to watch for key info updates.
    pub keyinfo_path: std::path::PathBuf,
}

/// An intervention injected from an external file.
#[derive(Debug, Clone)]
pub enum Intervention {
    InjectMessage(String),
    UpdateKeyInfo(String),
}

impl FileInterventionWatcher {
    /// Create a watcher with custom intervene and keyinfo file paths.
    pub fn new(
        intervene_path: impl Into<std::path::PathBuf>,
        keyinfo_path: impl Into<std::path::PathBuf>,
    ) -> Self {
        Self {
            intervene_path: intervene_path.into(),
            keyinfo_path: keyinfo_path.into(),
        }
    }

    /// Default paths under ~/.aegis/
    pub fn default_paths() -> Self {
        let base = aegis_types::paths::config_dir();
        Self {
            intervene_path: base.join("intervene.txt"),
            keyinfo_path: base.join("keyinfo.txt"),
        }
    }

    /// Poll for any pending intervention. Consumes the file on read.
    pub async fn poll(&self) -> Option<Intervention> {
        // Check intervene file first (higher priority)
        if self.intervene_path.exists() {
            if let Ok(content) = tokio::fs::read_to_string(&self.intervene_path).await {
                let content = content.trim().to_string();
                let _ = tokio::fs::remove_file(&self.intervene_path).await;
                if !content.is_empty() {
                    return Some(Intervention::InjectMessage(content));
                }
            }
        }
        // Check keyinfo file
        if self.keyinfo_path.exists() {
            if let Ok(content) = tokio::fs::read_to_string(&self.keyinfo_path).await {
                let content = content.trim().to_string();
                let _ = tokio::fs::remove_file(&self.keyinfo_path).await;
                if !content.is_empty() {
                    return Some(Intervention::UpdateKeyInfo(content));
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_outbound_message_new() {
        let msg = OutboundMessage::new("chat1", "hello");
        assert_eq!(msg.chat_id, "chat1");
        assert_eq!(msg.text, "hello");
        assert!(msg.is_final);
        assert!(msg.edit_message_id.is_none());
        assert!(msg.reply_to.is_none());
    }

    #[test]
    fn test_outbound_message_new_with_into() {
        let msg = OutboundMessage::new(String::from("chat2"), String::from("world"));
        assert_eq!(msg.chat_id, "chat2");
        assert_eq!(msg.text, "world");
    }

    #[test]
    fn test_inbound_message_clone_debug() {
        let msg = InboundMessage {
            channel: "telegram".into(),
            user_id: "u1".into(),
            chat_id: "c1".into(),
            text: "hi".into(),
        };
        let cloned = msg.clone();
        assert_eq!(cloned.channel, "telegram");
        let debug = format!("{:?}", msg);
        assert!(debug.contains("telegram"));
    }

    #[test]
    fn test_build_session_key_per_user() {
        let source = SessionSource {
            platform: "telegram".into(),
            chat_type: ChatType::Group,
            chat_id: "g1".into(),
            user_id: "u1".into(),
            thread_id: None,
        };
        let key = build_session_key(&source, SessionIsolation::PerUser);
        assert_eq!(key, "telegram:g1:u1:group");
    }

    #[test]
    fn test_build_session_key_shared() {
        let source = SessionSource {
            platform: "discord".into(),
            chat_type: ChatType::Channel,
            chat_id: "ch1".into(),
            user_id: "u1".into(),
            thread_id: None,
        };
        let key = build_session_key(&source, SessionIsolation::Shared);
        assert_eq!(key, "discord:ch1");
    }

    #[test]
    fn test_build_session_key_per_thread_with_id() {
        let source = SessionSource {
            platform: "slack".into(),
            chat_type: ChatType::Group,
            chat_id: "g1".into(),
            user_id: "u1".into(),
            thread_id: Some("t42".into()),
        };
        let key = build_session_key(&source, SessionIsolation::PerThread);
        assert_eq!(key, "slack:g1:t42");
    }

    #[test]
    fn test_build_session_key_per_thread_no_thread_id() {
        let source = SessionSource {
            platform: "slack".into(),
            chat_type: ChatType::Group,
            chat_id: "g1".into(),
            user_id: "u1".into(),
            thread_id: None,
        };
        let key = build_session_key(&source, SessionIsolation::PerThread);
        assert_eq!(key, "slack:g1:main");
    }

    #[test]
    fn test_build_session_key_private() {
        let source = SessionSource {
            platform: "telegram".into(),
            chat_type: ChatType::Private,
            chat_id: "p1".into(),
            user_id: "u1".into(),
            thread_id: None,
        };
        let key = build_session_key(&source, SessionIsolation::PerUser);
        assert_eq!(key, "telegram:p1:u1:private");
    }

    #[test]
    fn test_message_type_serde() {
        let t = MessageType::Text;
        let json = serde_json::to_string(&t).unwrap();
        assert_eq!(json, "\"text\"");
        let t2: MessageType = serde_json::from_str("\"photo\"").unwrap();
        assert!(matches!(t2, MessageType::Photo));
    }

    #[test]
    fn test_chat_type_serde() {
        let t = ChatType::Private;
        let json = serde_json::to_string(&t).unwrap();
        assert_eq!(json, "\"private\"");
    }

    #[test]
    fn test_session_isolation_serde() {
        let s = SessionIsolation::PerUser;
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, "\"per_user\"");
        let s2: SessionIsolation = serde_json::from_str("\"shared\"").unwrap();
        assert!(matches!(s2, SessionIsolation::Shared));
    }

    #[test]
    fn test_stream_consumer_new() {
        let sc = StreamConsumer::new(500);
        assert_eq!(sc.buffer, "");
        assert!(!sc.streaming_disabled);
        assert_eq!(sc.consecutive_failures, 0);
    }

    #[test]
    fn test_stream_consumer_push_delta() {
        let mut sc = StreamConsumer::new(0); // 0ms interval = always flush
        let should_flush = sc.push_delta("hello ");
        assert!(should_flush);
        assert_eq!(sc.buffer, "hello ");
        sc.push_delta("world");
        assert_eq!(sc.buffer, "hello world");
    }

    #[test]
    fn test_stream_consumer_failure_degrades() {
        let mut sc = StreamConsumer::new(100);
        assert!(sc.on_edit_failure()); // 1st failure
        assert!(sc.on_edit_failure()); // 2nd failure
        assert!(!sc.on_edit_failure()); // 3rd failure → disabled
        assert!(sc.streaming_disabled);
        // Once disabled, push_delta returns false
        assert!(!sc.push_delta("test"));
    }

    #[test]
    fn test_stream_consumer_success_resets() {
        let mut sc = StreamConsumer::new(100);
        sc.on_edit_failure();
        sc.on_edit_failure();
        sc.on_edit_success(); // reset
        assert_eq!(sc.consecutive_failures, 0);
        assert!(!sc.streaming_disabled);
    }

    #[test]
    fn test_stream_consumer_effective_interval() {
        let mut sc = StreamConsumer::new(500);
        // flood_backoff starts at 1s, min_edit_interval is 500ms → max = 1s
        assert_eq!(sc.effective_interval(), std::time::Duration::from_secs(1));

        // After failure, backoff increases (1s * 2 = 2s)
        sc.on_edit_failure();
        assert_eq!(sc.effective_interval(), std::time::Duration::from_secs(2));
    }

    #[test]
    fn test_file_intervention_watcher_new() {
        let watcher = FileInterventionWatcher::new("/tmp/intervene.txt", "/tmp/keyinfo.txt");
        assert_eq!(watcher.intervene_path, std::path::PathBuf::from("/tmp/intervene.txt"));
        assert_eq!(watcher.keyinfo_path, std::path::PathBuf::from("/tmp/keyinfo.txt"));
    }

    #[tokio::test]
    async fn test_file_intervention_watcher_poll_empty() {
        let dir = tempfile::tempdir().unwrap();
        let intervene = dir.path().join("intervene.txt");
        let keyinfo = dir.path().join("keyinfo.txt");
        let watcher = FileInterventionWatcher::new(&intervene, &keyinfo);
        assert!(watcher.poll().await.is_none());
    }

    #[tokio::test]
    async fn test_file_intervention_watcher_poll_intervene() {
        let dir = tempfile::tempdir().unwrap();
        let intervene = dir.path().join("intervene.txt");
        let keyinfo = dir.path().join("keyinfo.txt");
        std::fs::write(&intervene, "stop what you're doing").unwrap();
        let watcher = FileInterventionWatcher::new(&intervene, &keyinfo);
        let result = watcher.poll().await;
        assert!(result.is_some());
        match result.unwrap() {
            Intervention::InjectMessage(msg) => assert_eq!(msg, "stop what you're doing"),
            _ => panic!("Expected InjectMessage"),
        }
        // File should be consumed
        assert!(!intervene.exists());
    }

    #[tokio::test]
    async fn test_file_intervention_watcher_poll_keyinfo() {
        let dir = tempfile::tempdir().unwrap();
        let intervene = dir.path().join("intervene.txt");
        let keyinfo = dir.path().join("keyinfo.txt");
        std::fs::write(&keyinfo, "important context").unwrap();
        let watcher = FileInterventionWatcher::new(&intervene, &keyinfo);
        let result = watcher.poll().await;
        assert!(result.is_some());
        match result.unwrap() {
            Intervention::UpdateKeyInfo(msg) => assert_eq!(msg, "important context"),
            _ => panic!("Expected UpdateKeyInfo"),
        }
    }

    #[tokio::test]
    async fn test_file_intervention_watcher_poll_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let intervene = dir.path().join("intervene.txt");
        let keyinfo = dir.path().join("keyinfo.txt");
        std::fs::write(&intervene, "  ").unwrap(); // whitespace only
        let watcher = FileInterventionWatcher::new(&intervene, &keyinfo);
        assert!(watcher.poll().await.is_none());
    }

    #[test]
    fn test_intervention_clone_debug() {
        let i = Intervention::InjectMessage("test".into());
        let cloned = i.clone();
        let debug = format!("{:?}", cloned);
        assert!(debug.contains("InjectMessage"));
    }

    #[test]
    fn test_message_event_serde_roundtrip() {
        let event = MessageEvent {
            text: "hello".into(),
            message_type: MessageType::Text,
            source: SessionSource {
                platform: "telegram".into(),
                chat_type: ChatType::Private,
                chat_id: "c1".into(),
                user_id: "u1".into(),
                thread_id: None,
            },
            media_urls: vec![],
            reply_to: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        let decoded: MessageEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.text, "hello");
        assert_eq!(decoded.source.platform, "telegram");
    }
}
