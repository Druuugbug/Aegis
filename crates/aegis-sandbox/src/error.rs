//! Error types for `aegis-sandbox`.

use std::path::PathBuf;

/// A sandbox operation error.
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    /// The requested policy is not supported on this platform (and
    /// `allow_degrade` was `false`).
    #[error("sandbox not supported on this platform: {0}")]
    Unsupported(String),

    /// Applying landlock rules failed.
    #[error("landlock error: {0}")]
    Landlock(String),

    /// Applying seccomp filter failed.
    #[error("seccomp error: {0}")]
    Seccomp(String),

    /// A namespace operation (unshare, uid_map) failed.
    #[error("namespace error: {0}")]
    Namespace(String),

    /// An rlimit could not be applied.
    #[error("rlimit error: {0}")]
    Rlimit(String),

    /// The child process could not be spawned.
    #[error("spawn error: {0}")]
    Spawn(#[from] std::io::Error),

    /// A path required by the policy does not exist.
    #[error("policy path does not exist: {0}")]
    PathMissing(PathBuf),

    /// The policy is internally inconsistent (e.g. deny and allow overlap
    /// in an unresolvable way).
    #[error("invalid policy: {0}")]
    InvalidPolicy(String),
}

/// Convenience alias.
pub type SandboxResult<T> = Result<T, SandboxError>;
