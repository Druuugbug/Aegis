use aegis_core::config::Config;
use tokio::sync::{mpsc, oneshot};

/// Chat request sent to the agent thread.
pub struct ChatMessage {
    pub content: String,
    pub reply: oneshot::Sender<String>,
}

/// Shared application state accessible from all handlers.
///
/// The agent runs on a dedicated thread (rusqlite Connection isn't Send),
/// communicating via a channel. The store is opened on-demand per request.
pub struct AppState {
    /// Channel to send messages to the agent thread.
    pub agent_tx: mpsc::Sender<ChatMessage>,
    /// Application configuration.
    pub config: Config,
}

impl AppState {
    /// Creates a new `AppState` with the given agent channel and config.
    pub fn new(agent_tx: mpsc::Sender<ChatMessage>, config: Config) -> Self {
        Self { agent_tx, config }
    }
}
