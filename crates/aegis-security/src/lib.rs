//! # aegis-security
//!
//! Security sandbox and guardrails for Aegis agent execution.
//!
//! Provides defense-in-depth against common agent vulnerabilities:
//! - **Command audit**: pattern-based danger detection (rm -rf, fork bombs, etc.)
//! - **Path validation**: symlink escape prevention, workspace boundary enforcement
//! - **Credential sanitization**: API keys, tokens, private keys redaction
//! - **URL safety**: private IP and SSRF prevention
//! - **DLP filter**: credit card and sensitive data detection with redaction
//!
//! All checks produce structured [`ApprovalRequest`]s for human-in-the-loop workflows.

mod audit;
mod security;
pub mod identity;
pub mod permission;
pub mod rules;
pub mod secret_vault;

pub use audit::AuditLog;
pub use identity::{
    derive_sandbox_policy, identity_approval, is_read_only_tool, is_shell_execution_tool, Approval,
    Identity, TrustLevel,
};
pub use secret_vault::SecretVault;
pub use rules::{
    evaluate_permission, is_readonly_bash, Decision, PermissionMode, PermissionRule, RuleAction,
    RuleConfig, SecurityRulesConfig,
};
pub use security::{check_command, check_path, is_safe_url, sanitize_credentials, ApprovalRequest, DangerLevel, DlpAction, DlpFilter, DlpRule, DlpScanResult};
