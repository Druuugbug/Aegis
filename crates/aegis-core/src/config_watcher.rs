use std::path::PathBuf;
use std::time::SystemTime;
use tokio::sync::broadcast;

use crate::config::Config;

pub struct ConfigWatcher {
    path: PathBuf,
    last_mtime: SystemTime,
    tx: broadcast::Sender<Config>,
}

impl ConfigWatcher {
    /// Create a config watcher for the given file path, returning the watcher and a broadcast receiver.
    pub fn new(path: PathBuf) -> (Self, broadcast::Receiver<Config>) {
        let (tx, rx) = broadcast::channel(8);
        let last_mtime = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        (Self { path, last_mtime, tx }, rx)
    }

    /// Poll the config file for changes every 5 seconds, broadcasting updates.
    pub async fn watch(&mut self) {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            let mtime = match std::fs::metadata(&self.path).and_then(|m| m.modified()) {
                Ok(t) => t,
                Err(_) => continue,
            };
            if mtime != self.last_mtime {
                self.last_mtime = mtime;
                if let Ok(cfg) = Config::load(&self.path) {
                    let _ = self.tx.send(cfg);
                }
            }
        }
    }
}
