use std::collections::HashSet;

/// 7.1.1: Permission tier for tools
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PermissionTier {
    /// Runs silently with no notification
    Silent,
    /// Auto-approved
    Auto,
    /// User is notified but not blocked
    Notify,
    /// User must approve once per session
    Approve,
    /// User must confirm twice (double-confirm), even in YOLO mode
    Confirm2x,
}

/// 7.1.1: Get the permission tier for a tool by name
pub fn tool_permission_tier(tool_name: &str) -> PermissionTier {
    match tool_name {
        "terminal" | "write_file" | "patch" => PermissionTier::Approve,
        "browser" | "spawn_task" => PermissionTier::Notify,
        "read_file" | "search_files" => PermissionTier::Auto,
        _ => PermissionTier::Auto,
    }
}

/// 7.1.2: Check if a terminal command requires Confirm2x (double confirmation)
/// Matches dangerous patterns: rm -rf /, DROP TABLE, truncate, mkfs, dd if=
pub fn is_dangerous_command(cmd: &str) -> bool {
    let lower = cmd.to_lowercase();
    lower.contains("rm -rf /")
        || lower.contains("rm -rf ~")
        || lower.contains("rm -rf /*")
        || lower.contains("drop table")
        || lower.contains("drop database")
        || lower.contains("truncate")
        || lower.contains("mkfs")
        || lower.contains("dd if=")
        || lower.contains(":(){:|:&};:")  // fork bomb
}

/// 7.1.2: Upgrade tier for dangerous terminal commands
pub fn effective_tier(tool_name: &str, args: Option<&str>) -> PermissionTier {
    let base = tool_permission_tier(tool_name);
    if tool_name == "terminal" {
        if let Some(cmd) = args {
            if is_dangerous_command(cmd) {
                return PermissionTier::Confirm2x;
            }
        }
    }
    base
}

/// 7.1.3 & 7.1.4: Permission checker that handles YOLO mode and session memory
pub struct PermissionChecker {
    /// 7.1.4: Tools approved for this session (no re-prompt, except Confirm2x)
    session_approved: HashSet<String>,
    /// 7.1.3: YOLO mode skips Auto/Notify/Approve but NOT Confirm2x
    pub yolo: bool,
}

impl PermissionChecker {
    /// Creates a new `instance`.
    pub fn new(yolo: bool) -> Self {
        Self {
            session_approved: HashSet::new(),
            yolo,
        }
    }

    /// Returns true if the action is permitted without prompting.
    /// Confirm2x always requires explicit double-confirm (even in YOLO).
    pub fn is_auto_permitted(&self, tool_name: &str, tier: &PermissionTier) -> bool {
        match tier {
            PermissionTier::Silent | PermissionTier::Auto => true,
            PermissionTier::Notify => true, // notify but don't block
            PermissionTier::Approve => {
                // 7.1.4: session memory - already approved this session?
                self.yolo || self.session_approved.contains(tool_name)
            }
            PermissionTier::Confirm2x => false, // always prompt, 7.1.3
        }
    }

    /// Record that user has approved this tool for the session.
    pub fn record_approval(&mut self, tool_name: &str) {
        self.session_approved.insert(tool_name.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_permission_tier() {
        assert_eq!(tool_permission_tier("terminal"), PermissionTier::Approve);
        assert_eq!(tool_permission_tier("write_file"), PermissionTier::Approve);
        assert_eq!(tool_permission_tier("patch"), PermissionTier::Approve);
        assert_eq!(tool_permission_tier("browser"), PermissionTier::Notify);
        assert_eq!(tool_permission_tier("spawn_task"), PermissionTier::Notify);
        assert_eq!(tool_permission_tier("read_file"), PermissionTier::Auto);
        assert_eq!(tool_permission_tier("search_files"), PermissionTier::Auto);
        // Unknown tools default to Auto
        assert_eq!(tool_permission_tier("some_custom_tool"), PermissionTier::Auto);
    }

    #[test]
    fn test_is_dangerous_command() {
        assert!(is_dangerous_command("rm -rf /"));
        assert!(is_dangerous_command("rm -rf ~"));
        assert!(is_dangerous_command("rm -rf /*"));
        assert!(is_dangerous_command("DROP TABLE users"));
        assert!(is_dangerous_command("drop database prod"));
        assert!(is_dangerous_command("truncate table logs"));
        assert!(is_dangerous_command("mkfs.ext4 /dev/sda"));
        assert!(is_dangerous_command("dd if=/dev/zero of=/dev/sda"));
        assert!(is_dangerous_command(":(){:|:&};:"));

        assert!(!is_dangerous_command("ls -la"));
        assert!(!is_dangerous_command("echo hello"));
        assert!(!is_dangerous_command("git status"));
    }

    #[test]
    fn test_effective_tier_normal() {
        assert_eq!(effective_tier("read_file", None), PermissionTier::Auto);
        assert_eq!(effective_tier("terminal", Some("ls -la")), PermissionTier::Approve);
        assert_eq!(effective_tier("terminal", None), PermissionTier::Approve);
    }

    #[test]
    fn test_effective_tier_dangerous_command() {
        assert_eq!(effective_tier("terminal", Some("rm -rf /")), PermissionTier::Confirm2x);
        assert_eq!(effective_tier("terminal", Some("DROP TABLE users")), PermissionTier::Confirm2x);
        assert_eq!(effective_tier("terminal", Some("mkfs.ext4 /dev/sda")), PermissionTier::Confirm2x);
    }

    #[test]
    fn test_effective_tier_non_terminal_tool() {
        // Even a dangerous-looking command in a non-terminal tool should not upgrade
        assert_eq!(effective_tier("browser", Some("rm -rf /")), PermissionTier::Notify);
    }

    #[test]
    fn test_permission_checker_yolo_mode() {
        let checker = PermissionChecker::new(true);
        assert!(checker.is_auto_permitted("terminal", &PermissionTier::Approve));
        assert!(checker.is_auto_permitted("read_file", &PermissionTier::Auto));
        assert!(checker.is_auto_permitted("browser", &PermissionTier::Notify));
        // Confirm2x is never auto-permitted, even in YOLO
        assert!(!checker.is_auto_permitted("terminal", &PermissionTier::Confirm2x));
    }

    #[test]
    fn test_permission_checker_session_approved() {
        let mut checker = PermissionChecker::new(false);
        // Before approval, Approve tier is not auto-permitted
        assert!(!checker.is_auto_permitted("terminal", &PermissionTier::Approve));

        checker.record_approval("terminal");
        // After approval, Approve tier is auto-permitted
        assert!(checker.is_auto_permitted("terminal", &PermissionTier::Approve));

        // But Confirm2x is still not auto-permitted
        assert!(!checker.is_auto_permitted("terminal", &PermissionTier::Confirm2x));
    }

    #[test]
    fn test_permission_checker_silent_auto_notify() {
        let checker = PermissionChecker::new(false);
        assert!(checker.is_auto_permitted("any", &PermissionTier::Silent));
        assert!(checker.is_auto_permitted("any", &PermissionTier::Auto));
        assert!(checker.is_auto_permitted("any", &PermissionTier::Notify));
    }
}
