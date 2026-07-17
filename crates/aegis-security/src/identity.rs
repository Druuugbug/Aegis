//! Identity, trust levels, and identity → sandbox policy derivation.
//!
//! This module is the **single authoritative entry point** for sandbox
//! policy selection in Aegis. Every tool that spawns a subprocess must go
//! through [`derive_sandbox_policy`] to translate "who is calling" into
//! "what the OS should allow".
//!
//! See `devdocs/design-sandbox.md` §"身份感知的权限体系" for the model.

use aegis_sandbox::{presets, SandboxPolicy};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Who is invoking a tool call. Every tool entry point must receive one.
///
/// - `LocalOwner`: identified by Unix-socket peer credentials matching the
///   parent Aegis process's uid.
/// - `A2aPeer`: identified by a verified `CapabilityToken` from
///   `aegis-a2a`. `trust` comes from the local `peers.toml`.
/// - `Channel`: identified by (channel, chat_id, user_id). `trust` from
///   `~/.aegis/channel_trust.toml`.
/// - `McpServer`: identified by config-declared server name.
/// - `Internal`: Aegis-owned subsystem calls (scheduler, evolve, ...).
#[derive(Debug, Clone)]
pub enum Identity {
    /// The user themselves at the local CLI or a gateway client on the same
    /// machine.
    LocalOwner,

    /// A remote A2A peer agent.
    A2aPeer {
        /// The peer's `agent_id` claim.
        agent_id: String,
        /// Human-readable capabilities from the token (for UI/audit).
        capabilities: Vec<String>,
        /// Locally-assigned trust.
        trust: TrustLevel,
    },

    /// A channel-originated call (Telegram/Discord/Slack/Feishu).
    Channel {
        /// Which channel platform.
        channel: String,
        /// Chat identifier.
        chat_id: String,
        /// Whether this is a group chat.
        is_group: bool,
        /// Locally-assigned trust.
        trust: TrustLevel,
    },

    /// An MCP server subprocess.
    McpServer {
        /// Server name from `[mcp_servers.NAME]`.
        server_name: String,
        /// Locally-assigned trust.
        trust: TrustLevel,
    },

    /// Aegis-internal subsystem. Always fully trusted.
    Internal {
        /// Subsystem name for audit.
        subsystem: &'static str,
    },
}

/// Five trust tiers, ordered from most to least trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TrustLevel {
    /// Full authority, equivalent to owner. Sandbox: `unrestricted`.
    Owner,
    /// Trusted internal peer (LAN, private channel). Sandbox:
    /// `compute_workdir` for shell tools.
    Trusted,
    /// Standard peer / DM. Sandbox: `compute_workdir` + deny sensitive HOME
    /// paths (`~/.ssh`, `~/.aws`, `~/.gnupg`).
    Standard,
    /// Group chat / unknown peer. Cannot invoke shell tools. Sandbox:
    /// `parser_offline`.
    Restricted,
    /// Read-only. Any spawning tool is denied at policy-derivation time.
    ReadOnly,
}

impl Default for TrustLevel {
    fn default() -> Self {
        // A newly-connected caller with no explicit trust assignment gets
        // the safest useful tier: read-only.
        TrustLevel::ReadOnly
    }
}

impl std::fmt::Display for TrustLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            TrustLevel::Owner => "owner",
            TrustLevel::Trusted => "trusted",
            TrustLevel::Standard => "standard",
            TrustLevel::Restricted => "restricted",
            TrustLevel::ReadOnly => "read_only",
        };
        f.write_str(s)
    }
}

impl std::str::FromStr for TrustLevel {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "owner" => Ok(TrustLevel::Owner),
            "trusted" => Ok(TrustLevel::Trusted),
            "standard" => Ok(TrustLevel::Standard),
            "restricted" => Ok(TrustLevel::Restricted),
            "readonly" | "read_only" | "read-only" => Ok(TrustLevel::ReadOnly),
            other => Err(format!(
                "unknown trust level '{other}' (expected: owner|trusted|standard|restricted|read_only)"
            )),
        }
    }
}

impl Identity {
    /// Trust level for this identity. Owner and Internal are always Owner;
    /// others carry a stored level.
    pub fn trust_level(&self) -> TrustLevel {
        match self {
            Identity::LocalOwner => TrustLevel::Owner,
            Identity::Internal { .. } => TrustLevel::Owner,
            Identity::A2aPeer { trust, .. }
            | Identity::Channel { trust, .. }
            | Identity::McpServer { trust, .. } => *trust,
        }
    }

    /// Whether YOLO (skip-approvals) is honored for this identity.
    ///
    /// Only the local user physically at the CLI can bypass approvals —
    /// remote peers/channels never can, even if they carry a token claiming
    /// so. This blocks "remote peer says it's YOLO, so no prompts needed".
    pub fn honors_yolo(&self) -> bool {
        matches!(self, Identity::LocalOwner | Identity::Internal { .. })
    }

    /// Short human-readable identifier for audit logs.
    pub fn display(&self) -> String {
        match self {
            Identity::LocalOwner => "local-owner".into(),
            Identity::A2aPeer { agent_id, .. } => format!("a2a:{agent_id}"),
            Identity::Channel {
                channel, chat_id, ..
            } => {
                format!("channel:{channel}/{chat_id}")
            }
            Identity::McpServer { server_name, .. } => format!("mcp:{server_name}"),
            Identity::Internal { subsystem } => format!("internal:{subsystem}"),
        }
    }
}

// ─── Approval decisions ────────────────────────────────────────────────────

/// Approval decision from [`identity_approval`].
///
/// Layers on top of the existing `PermissionTier` — the caller combines
/// this with the tier logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    /// Auto-approve; no user interaction.
    Silent,
    /// Ask the user (via the existing approval callback).
    Ask,
    /// Hard-deny; must not invoke the tool.
    Deny,
}

/// Return the identity-based approval decision for a `(identity, tool)`
/// pair, given whether YOLO mode is set globally.
///
/// Rules:
/// - `ReadOnly`: only read-only tools allowed; everything else is `Deny`.
/// - `Restricted`: shell-execution tools (`terminal`, `browser`, `remote`)
///   are `Deny`; other tools go through `Ask`.
/// - `Standard`: write-ish tools always `Ask`, regardless of YOLO.
/// - `Trusted`: write-ish tools `Ask`, read-ish `Silent`.
/// - `Owner`: YOLO controls; if YOLO, `Silent`; else `Ask`.
///
/// The caller may further tighten via `PermissionTier`/rules but should not
/// widen the returned decision (a `Deny` must be honored).
pub fn identity_approval(identity: &Identity, tool: &str, yolo_global: bool) -> Approval {
    let trust = identity.trust_level();
    let read_only_tool = is_read_only_tool(tool);
    let shell_tool = is_shell_execution_tool(tool);

    // Hard denies first.
    if matches!(trust, TrustLevel::ReadOnly) && !read_only_tool {
        return Approval::Deny;
    }
    if matches!(trust, TrustLevel::Restricted) && shell_tool {
        return Approval::Deny;
    }

    // YOLO only helps the local owner.
    let yolo = yolo_global && identity.honors_yolo();

    match trust {
        TrustLevel::Owner => {
            if yolo {
                Approval::Silent
            } else if read_only_tool {
                Approval::Silent
            } else {
                Approval::Ask
            }
        }
        TrustLevel::Trusted => {
            if read_only_tool {
                Approval::Silent
            } else {
                Approval::Ask
            }
        }
        TrustLevel::Standard => {
            if read_only_tool {
                Approval::Silent
            } else {
                Approval::Ask
            }
        }
        TrustLevel::Restricted => Approval::Ask,
        TrustLevel::ReadOnly => Approval::Silent, // (read-only tools only)
    }
}

// ─── Policy derivation ─────────────────────────────────────────────────────

/// Derive the sandbox policy for a `(identity, tool, cwd)` triple.
///
/// This is the **single authoritative entry point** for sandbox policy
/// selection. All tool code that spawns child processes must go through it.
///
/// - Trust level determines the baseline preset shape.
/// - Tool name selects between compute and network variants.
/// - Non-Owner identities layer an explicit deny of sensitive HOME paths.
/// - `ReadOnly` and shell tools under `Restricted` return `deny_all`.
///
/// This function never *widens* — callers can further intersect the result
/// with a `CapabilityToken` allow-list to narrow it more.
pub fn derive_sandbox_policy(identity: &Identity, tool: &str, cwd: &Path) -> SandboxPolicy {
    let trust = identity.trust_level();

    // Fast-path denials.
    if matches!(trust, TrustLevel::ReadOnly) {
        return presets::deny_all();
    }
    if matches!(trust, TrustLevel::Restricted) && is_shell_execution_tool(tool) {
        return presets::deny_all();
    }

    match trust {
        TrustLevel::Owner => presets::unrestricted(),
        TrustLevel::Trusted => tool_preset(tool, cwd),
        TrustLevel::Standard => tool_preset(tool, cwd).with_extra_deny(sensitive_home_paths()),
        TrustLevel::Restricted => {
            // Restricted + non-shell tool: read-only sandbox.
            presets::parser_offline(&[cwd.to_path_buf()]).with_extra_deny(sensitive_home_paths())
        }
        TrustLevel::ReadOnly => presets::deny_all(),
    }
}

/// Pick the tool-shape-appropriate preset for the given tool.
fn tool_preset(tool: &str, cwd: &Path) -> SandboxPolicy {
    match tool {
        // Shell-execution tools need workdir write + no network by default.
        "terminal" | "spawn_task" => presets::compute_workdir(cwd),
        // Tools that fetch from the network need network on.
        "web_extract" | "browser" | "web_search" => presets::network_readonly(cwd),
        // Everything else: safest useful shape.
        _ => presets::compute_workdir(cwd),
    }
}

/// Tools that spawn shell/remote code execution. These are hard-denied for
/// Restricted trust and require approval for anything below Owner.
pub fn is_shell_execution_tool(tool: &str) -> bool {
    matches!(tool, "terminal" | "browser" | "remote" | "background")
}

/// Tools that never modify state and are safe under any trust level.
pub fn is_read_only_tool(tool: &str) -> bool {
    matches!(
        tool,
        "read_file"
            | "search_files"
            | "session_search"
            | "memory_search"
            | "record_search"
            | "todo"
            | "clarify"
            | "web_search"
    )
}

/// Paths in `$HOME` that should be denied for any non-Owner identity.
///
/// Deny is defense-in-depth on top of landlock: even if the caller listed
/// `$HOME` as an ro path, these child paths still won't resolve.
fn sensitive_home_paths() -> Vec<PathBuf> {
    let home = match std::env::var_os("HOME") {
        Some(h) => PathBuf::from(h),
        None => return Vec::new(),
    };
    vec![
        home.join(".ssh"),
        home.join(".aws"),
        home.join(".gnupg"),
        home.join(".azure"),
        home.join(".gcp"),
        home.join(".kube"),
        home.join(".docker"),
        home.join(".config").join("aegis"),
        home.join(".aegis"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_owner_is_always_owner() {
        assert_eq!(Identity::LocalOwner.trust_level(), TrustLevel::Owner);
    }

    #[test]
    fn a2a_peer_carries_trust() {
        let id = Identity::A2aPeer {
            agent_id: "foo".into(),
            capabilities: vec![],
            trust: TrustLevel::Standard,
        };
        assert_eq!(id.trust_level(), TrustLevel::Standard);
    }

    #[test]
    fn yolo_honored_only_by_owner() {
        assert!(Identity::LocalOwner.honors_yolo());
        assert!(Identity::Internal { subsystem: "test" }.honors_yolo());
        let peer = Identity::A2aPeer {
            agent_id: "x".into(),
            capabilities: vec![],
            trust: TrustLevel::Trusted,
        };
        assert!(!peer.honors_yolo());
    }

    #[test]
    fn owner_gets_unrestricted() {
        let p = derive_sandbox_policy(&Identity::LocalOwner, "terminal", Path::new("/tmp"));
        // unrestricted = default policy, no deny_all
        assert!(!p.deny_all);
    }

    #[test]
    fn readonly_returns_deny_all_for_any_tool() {
        let id = Identity::A2aPeer {
            agent_id: "x".into(),
            capabilities: vec![],
            trust: TrustLevel::ReadOnly,
        };
        let p = derive_sandbox_policy(&id, "read_file", Path::new("/tmp"));
        assert!(p.deny_all);
    }

    #[test]
    fn restricted_shell_tool_denied() {
        let id = Identity::Channel {
            channel: "feishu".into(),
            chat_id: "grp1".into(),
            is_group: true,
            trust: TrustLevel::Restricted,
        };
        let p = derive_sandbox_policy(&id, "terminal", Path::new("/tmp"));
        assert!(p.deny_all);
    }

    #[test]
    fn approval_readonly_denies_terminal() {
        let id = Identity::A2aPeer {
            agent_id: "x".into(),
            capabilities: vec![],
            trust: TrustLevel::ReadOnly,
        };
        assert_eq!(identity_approval(&id, "terminal", true), Approval::Deny);
    }

    #[test]
    fn approval_yolo_ignored_for_non_owner() {
        let id = Identity::A2aPeer {
            agent_id: "x".into(),
            capabilities: vec![],
            trust: TrustLevel::Trusted,
        };
        // Trusted + YOLO=true → still Ask (yolo only helps owner)
        assert_eq!(identity_approval(&id, "terminal", true), Approval::Ask);
    }

    #[test]
    fn approval_owner_yolo_is_silent() {
        assert_eq!(
            identity_approval(&Identity::LocalOwner, "terminal", true),
            Approval::Silent
        );
    }

    #[test]
    fn approval_owner_no_yolo_asks_for_write() {
        assert_eq!(
            identity_approval(&Identity::LocalOwner, "terminal", false),
            Approval::Ask
        );
    }

    #[test]
    fn trust_level_from_str_roundtrip() {
        for &t in &[
            TrustLevel::Owner,
            TrustLevel::Trusted,
            TrustLevel::Standard,
            TrustLevel::Restricted,
            TrustLevel::ReadOnly,
        ] {
            let parsed: TrustLevel = t.to_string().parse().expect("roundtrip");
            assert_eq!(parsed, t);
        }
    }
}
