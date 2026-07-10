use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapState {
    pub session_id: String,
    pub reason: SwapReason,
    pub timestamp: String,
    #[serde(default)]
    pub context_summary: Option<String>,
    #[serde(default = "default_replay")]
    pub replay_messages: u32,
    #[serde(default)]
    pub new_binary_path: Option<String>,
    #[serde(default)]
    pub previous_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SwapReason {
    HotCompile,
    HotUpgrade,
    Restart,
}

impl std::fmt::Display for SwapReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HotCompile => write!(f, "热编译"),
            Self::HotUpgrade => write!(f, "热升级"),
            Self::Restart => write!(f, "重启"),
        }
    }
}

fn default_replay() -> u32 {
    10
}

fn state_path() -> PathBuf {
    crate::config::config_dir().join("swap-state.json")
}

impl SwapState {
    pub fn new(session_id: impl Into<String>, reason: SwapReason) -> Self {
        Self {
            session_id: session_id.into(),
            reason,
            timestamp: chrono::Utc::now().to_rfc3339(),
            context_summary: None,
            replay_messages: default_replay(),
            new_binary_path: None,
            previous_version: None,
        }
    }

    pub fn with_summary(mut self, summary: impl Into<String>) -> Self {
        self.context_summary = Some(summary.into());
        self
    }

    pub fn with_replay_count(mut self, n: u32) -> Self {
        self.replay_messages = n;
        self
    }

    pub fn with_new_binary(mut self, path: impl Into<String>) -> Self {
        self.new_binary_path = Some(path.into());
        self
    }

    pub fn with_previous_version(mut self, version: impl Into<String>) -> Self {
        self.previous_version = Some(version.into());
        self
    }
}

/// Write swap state to disk before exec/shutdown.
pub fn save(state: &SwapState) -> Result<()> {
    let path = state_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(state)?;
    std::fs::write(&path, json)?;
    Ok(())
}

/// Read swap state after startup. Returns None if file doesn't exist.
pub fn load() -> Option<SwapState> {
    let path = state_path();
    let content = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Remove swap state file after successful recovery.
pub fn clear() {
    let _ = std::fs::remove_file(state_path());
}

/// Generate a preamble message for the model after hot-swap recovery.
pub fn recovery_preamble(state: &SwapState) -> String {
    let mut preamble = format!("🔄 aegis 已完成{}，对话无缝继续。", state.reason);
    if let Some(ref summary) = state.context_summary {
        preamble.push_str(&format!("\n\n对话摘要：{}", summary));
    }
    preamble.push_str("\n\n继续响应用户，无需提及升级过程（除非用户问起）。");
    preamble
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn test_swap_state_serde_roundtrip() {
        let state = SwapState::new("sess-abc123", SwapReason::HotCompile)
            .with_summary("用户正在部署 staging")
            .with_replay_count(15);

        let json = serde_json::to_string(&state).unwrap();
        let back: SwapState = serde_json::from_str(&json).unwrap();

        assert_eq!(back.session_id, "sess-abc123");
        assert_eq!(back.reason, SwapReason::HotCompile);
        assert_eq!(back.replay_messages, 15);
        assert_eq!(
            back.context_summary.as_deref(),
            Some("用户正在部署 staging")
        );
    }

    #[test]
    fn test_swap_reason_display() {
        assert_eq!(format!("{}", SwapReason::HotCompile), "热编译");
        assert_eq!(format!("{}", SwapReason::HotUpgrade), "热升级");
        assert_eq!(format!("{}", SwapReason::Restart), "重启");
    }

    #[test]
    fn test_recovery_preamble_basic() {
        let state = SwapState::new("sess-1", SwapReason::HotUpgrade);
        let p = recovery_preamble(&state);
        assert!(p.contains("热升级"));
        assert!(!p.contains("对话摘要"));
    }

    #[test]
    fn test_recovery_preamble_with_summary() {
        let state =
            SwapState::new("sess-2", SwapReason::HotCompile).with_summary("用户在修改 widget 系统");
        let p = recovery_preamble(&state);
        assert!(p.contains("热编译"));
        assert!(p.contains("用户在修改 widget 系统"));
    }

    #[test]
    fn test_save_load_clear() {
        // Use a temp dir so we don't pollute real config
        let tmp = env::temp_dir().join("aegis-test-swap-state");
        let _ = std::fs::create_dir_all(&tmp);
        env::set_var("AEGIS_HOME", &tmp);

        let state = SwapState::new("sess-test", SwapReason::Restart);
        save(&state).unwrap();

        let loaded = load().unwrap();
        assert_eq!(loaded.session_id, "sess-test");
        assert_eq!(loaded.reason, SwapReason::Restart);

        clear();
        assert!(load().is_none());

        env::remove_var("AEGIS_HOME");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
