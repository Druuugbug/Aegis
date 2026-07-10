//! # aegis-sandbox
//!
//! Minimal user-space embedded sandbox for Aegis tool execution.
//!
//! Provides defense-in-depth for spawned child processes that run
//! external/untrusted input (terminal commands, web-fetched content parsing,
//! MCP servers) — while keeping the main Aegis process unsandboxed so it can
//! operate the host system freely.
//!
//! ## Layers
//!
//! On Linux, `LinuxRunner` composes three kernel-level primitives:
//!
//! - **landlock** — filesystem access allowlist
//! - **seccomp** — syscall allow/deny filter
//! - **user namespace** — uid mapping (child sees uid=nobody)
//!
//! plus **rlimit** for CPU/memory/fd caps.
//!
//! On unsupported platforms, `NoopRunner` runs commands unchanged (equivalent
//! to today's behavior — the sandbox is strictly opt-in via config).
//!
//! ## Not this crate's job
//!
//! Deciding *what policy to apply for a given caller* lives in
//! `aegis-security::identity::derive_sandbox_policy`. This crate only executes
//! a policy against a subprocess.
//!
//! See `devdocs/design-sandbox.md` for the full design.

#![warn(missing_docs)]

mod error;
mod hardening;
mod policy;
pub mod presets;
mod runner;

#[cfg(target_os = "linux")]
mod linux;

pub use error::{SandboxError, SandboxResult};
pub use hardening::pre_main_hardening;
pub use policy::{
    FsPolicy, HostPort, NetworkPolicy, ResourceLimits, SandboxPolicy, SyscallProfile, UidPolicy,
};
pub use runner::{NoopRunner, RunnerCapabilities, SandboxChild, SandboxCommand, SandboxRunner};

#[cfg(target_os = "linux")]
pub use linux::{apply_policy_pre_exec, LinuxRunner};

use std::sync::Arc;

/// Return the best available runner for the current platform.
///
/// - On Linux with kernel ≥ 5.13, returns [`LinuxRunner`] boxed as a trait
///   object.
/// - Elsewhere (or when Linux capabilities are missing), returns [`NoopRunner`].
///
/// The caller can also construct a specific runner directly if it wants finer
/// control over degradation behavior.
pub fn default_runner() -> Arc<dyn SandboxRunner> {
    #[cfg(target_os = "linux")]
    {
        match LinuxRunner::new() {
            Ok(r) => return Arc::new(r),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "Linux sandbox unavailable, falling back to NoopRunner"
                );
            }
        }
    }
    Arc::new(NoopRunner::new())
}
