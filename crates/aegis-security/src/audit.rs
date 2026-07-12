use anyhow::Result;
use chrono::Utc;
use serde::Serialize;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

/// Append-only audit log for all agent actions.
pub struct AuditLog {
    path: PathBuf,
    /// Rotate at this size (bytes); `0` = never rotate.
    max_bytes: u64,
}

#[derive(Serialize)]
struct AuditEntry {
    timestamp: String,
    agent_id: String,
    action: String,
    detail: String,
    approved: Option<bool>,
}

impl AuditLog {
    /// Creates a new `instance`.
    pub fn new() -> Self {
        Self::with_max_mb(5)
    }

    /// Create an audit log with a configurable rotation cap (MB; `0` = no cap).
    pub fn with_max_mb(max_mb: u64) -> Self {
        let dir = aegis_types::paths::config_dir().join("logs");
        let _ = std::fs::create_dir_all(&dir);
        Self {
            path: dir.join("audit.log"),
            max_bytes: max_mb.saturating_mul(1024 * 1024),
        }
    }

    /// Log a tool execution.
    pub fn log_tool(&self, agent_id: &str, tool_name: &str, args: &str, approved: bool) {
        let _ = self.append(AuditEntry {
            timestamp: Utc::now().to_rfc3339(),
            agent_id: agent_id.to_string(),
            action: format!("tool:{tool_name}"),
            detail: args[..args.floor_char_boundary(500)].to_string(),
            approved: Some(approved),
        });
    }

    /// Log a generic action.
    pub fn log_action(&self, agent_id: &str, action: &str, detail: &str) {
        let _ = self.append(AuditEntry {
            timestamp: Utc::now().to_rfc3339(),
            agent_id: agent_id.to_string(),
            action: action.to_string(),
            detail: detail[..detail.floor_char_boundary(500)].to_string(),
            approved: None,
        });
    }

    fn append(&self, entry: AuditEntry) -> Result<()> {
        // Size-bound (the daemon is long-lived): rotate at the cap, keep one
        // backup (`audit.log.1`) so the audit trail never grows without limit.
        if self.max_bytes > 0 {
            if let Ok(meta) = std::fs::metadata(&self.path) {
                if meta.len() > self.max_bytes {
                    let _ = std::fs::rename(&self.path, self.path.with_extension("log.1"));
                }
            }
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(file, "{}", serde_json::to_string(&entry)?)?;
        Ok(())
    }
}

impl Default for AuditLog {
    fn default() -> Self {
        Self::new()
    }
}
