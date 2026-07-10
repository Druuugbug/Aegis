//! Declarative sandbox policy types.
//!
//! A [`SandboxPolicy`] describes *what* a sandboxed child process is allowed
//! to do. Runners (see `runner.rs`) turn that description into concrete OS
//! calls.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Filesystem access declaration for the sandboxed child.
///
/// Semantics:
/// - `ro`/`rw` are additive allowlists (child can read/write there and their
///   descendants).
/// - `deny` overrides allowlists (used to punch a hole in a broad allow).
/// - Paths not listed in any list are **denied by default** when landlock is
///   active; they behave normally under [`NoopRunner`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FsPolicy {
    /// Read-only allowed paths.
    #[serde(default)]
    pub ro: Vec<PathBuf>,
    /// Read+write allowed paths.
    #[serde(default)]
    pub rw: Vec<PathBuf>,
    /// Explicit deny paths (override allows).
    #[serde(default)]
    pub deny: Vec<PathBuf>,
    /// Whether the child may create new regular files / directories inside
    /// `rw` paths. When false, only overwrites of existing files are allowed.
    #[serde(default = "d_true")]
    pub allow_create: bool,
}

fn d_true() -> bool {
    true
}

/// Named syscall configuration profiles.
///
/// Each profile is a coarse-grained bundle; finer control belongs in Phase 2
/// (`SyscallProfile::Custom`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyscallProfile {
    /// No seccomp filter installed (rely on other layers).
    #[default]
    Unrestricted,
    /// Pure computation + file I/O + signals + time. Blocks: ptrace,
    /// kexec_load, init_module, bpf, mount, unshare(new_user),
    /// keyctl, add_key, and other rare/dangerous syscalls.
    Compute,
    /// `Compute` + socket/connect/send/recv (used when `NetworkPolicy::Host`
    /// or `NetworkPolicy::Allowlist` is set).
    ComputeNet,
}

/// Network access policy.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkPolicy {
    /// Inherit the host's network stack (child can reach anywhere the parent
    /// can reach).
    #[default]
    Host,
    /// Cut all network access via `unshare(CLONE_NEWNET)` — only loopback,
    /// which is not brought up.
    None,
    /// Host-level firewall / proxy allowlist (Phase 3 — not implemented in
    /// this crate yet; equivalent to `Host` at runtime).
    Allowlist(Vec<HostPort>),
}

/// A `host:port` pair for network allowlists.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostPort {
    /// DNS name or IP.
    pub host: String,
    /// TCP port.
    pub port: u16,
}

/// UID mapping policy for the child.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UidPolicy {
    /// Keep the parent's real uid/gid.
    #[default]
    Keep,
    /// Enter a new user namespace and map current uid → nobody (65534),
    /// current gid → nogroup (65534). Requires unprivileged user namespaces
    /// (Linux ≥ 3.8, enabled by default on modern kernels; Ubuntu 24.04+ may
    /// need an AppArmor tweak).
    MapToNobody,
}

/// Per-child resource limits enforced via `setrlimit(2)` + a wall-clock
/// watchdog.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResourceLimits {
    /// RLIMIT_CPU (seconds of CPU time).
    #[serde(default)]
    pub max_cpu_seconds: Option<u64>,
    /// RLIMIT_AS (address-space size in bytes).
    #[serde(default)]
    pub max_memory_bytes: Option<u64>,
    /// RLIMIT_NOFILE (open file descriptor count).
    #[serde(default)]
    pub max_open_files: Option<u64>,
    /// Externally enforced wall-clock limit (seconds). When the timer fires,
    /// the runner SIGKILLs the whole process group.
    #[serde(default)]
    pub max_wall_seconds: Option<u64>,
}

/// Complete sandbox policy for a single child spawn.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SandboxPolicy {
    /// Filesystem access declaration.
    #[serde(default)]
    pub fs: FsPolicy,
    /// Syscall profile.
    #[serde(default)]
    pub syscalls: SyscallProfile,
    /// Network policy.
    #[serde(default)]
    pub network: NetworkPolicy,
    /// UID mapping policy.
    #[serde(default)]
    pub uid: UidPolicy,
    /// Resource limits.
    #[serde(default)]
    pub resource: ResourceLimits,
    /// When true, unsupported features degrade to noop with a warning. When
    /// false (default), `spawn` returns an error on platforms that cannot
    /// enforce the policy.
    #[serde(default)]
    pub allow_degrade: bool,
    /// When true, this policy explicitly denies *any* spawn. Used by
    /// `derive_sandbox_policy` for the `ReadOnly` trust level: rejection
    /// happens at policy-derivation time, before the runner is invoked.
    ///
    /// A runner that receives a policy with `deny_all = true` MUST return an
    /// error from `spawn` and MUST NOT execute the command.
    #[serde(default)]
    pub deny_all: bool,
}

impl SandboxPolicy {
    /// Return a new policy with landlock `deny` list extended by `extra`.
    /// Useful for layering (e.g. always deny `~/.ssh` on non-Owner identities).
    #[must_use]
    pub fn with_extra_deny<I, P>(mut self, extra: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        for p in extra {
            self.fs.deny.push(p.into());
        }
        self
    }

    /// Intersect this policy with `other` — the result is at most as
    /// permissive as either input. Used to combine A2A `CapabilityToken`
    /// declared limits with the trust-level baseline.
    ///
    /// Rules:
    /// - fs.ro / fs.rw: intersect (path must appear in both to survive)
    /// - fs.deny: union (any deny from either side wins)
    /// - syscalls / network: take the stricter of the two (higher enum ord)
    /// - deny_all: OR
    /// - resource: take the smaller (more restrictive) of each limit
    #[must_use]
    pub fn intersect(mut self, other: &Self) -> Self {
        self.fs.ro = intersect_paths(&self.fs.ro, &other.fs.ro);
        self.fs.rw = intersect_paths(&self.fs.rw, &other.fs.rw);
        self.fs.deny.extend(other.fs.deny.iter().cloned());
        self.fs.allow_create = self.fs.allow_create && other.fs.allow_create;
        self.syscalls = stricter_syscalls(self.syscalls, other.syscalls.clone());
        self.network = stricter_network(self.network, other.network.clone());
        // uid: MapToNobody is stricter than Keep
        if matches!(other.uid, UidPolicy::MapToNobody) {
            self.uid = UidPolicy::MapToNobody;
        }
        self.resource = stricter_resource(&self.resource, &other.resource);
        self.deny_all = self.deny_all || other.deny_all;
        self
    }
}

fn intersect_paths(a: &[PathBuf], b: &[PathBuf]) -> Vec<PathBuf> {
    a.iter().filter(|p| b.contains(p)).cloned().collect()
}

fn stricter_syscalls(a: SyscallProfile, b: SyscallProfile) -> SyscallProfile {
    // Compute is stricter than ComputeNet is stricter than Unrestricted.
    use SyscallProfile::{Compute, ComputeNet, Unrestricted};
    match (a, b) {
        (Compute, _) | (_, Compute) => Compute,
        (ComputeNet, _) | (_, ComputeNet) => ComputeNet,
        _ => Unrestricted,
    }
}

fn stricter_network(a: NetworkPolicy, b: NetworkPolicy) -> NetworkPolicy {
    // None is strictest; Allowlist is stricter than Host.
    use NetworkPolicy::{Allowlist, Host, None as N};
    match (a, b) {
        (N, _) | (_, N) => N,
        (Allowlist(x), _) => Allowlist(x),
        (_, Allowlist(x)) => Allowlist(x),
        _ => Host,
    }
}

fn stricter_resource(a: &ResourceLimits, b: &ResourceLimits) -> ResourceLimits {
    fn min_opt(x: Option<u64>, y: Option<u64>) -> Option<u64> {
        match (x, y) {
            (Some(x), Some(y)) => Some(x.min(y)),
            (Some(x), None) | (None, Some(x)) => Some(x),
            (None, None) => None,
        }
    }
    ResourceLimits {
        max_cpu_seconds: min_opt(a.max_cpu_seconds, b.max_cpu_seconds),
        max_memory_bytes: min_opt(a.max_memory_bytes, b.max_memory_bytes),
        max_open_files: min_opt(a.max_open_files, b.max_open_files),
        max_wall_seconds: min_opt(a.max_wall_seconds, b.max_wall_seconds),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_is_permissive() {
        let p = SandboxPolicy::default();
        assert!(!p.deny_all);
        assert!(matches!(p.syscalls, SyscallProfile::Unrestricted));
        assert!(matches!(p.network, NetworkPolicy::Host));
        assert!(matches!(p.uid, UidPolicy::Keep));
    }

    #[test]
    fn intersect_takes_stricter_syscalls() {
        let a = SandboxPolicy {
            syscalls: SyscallProfile::ComputeNet,
            ..Default::default()
        };
        let b = SandboxPolicy {
            syscalls: SyscallProfile::Compute,
            ..Default::default()
        };
        let c = a.intersect(&b);
        assert!(matches!(c.syscalls, SyscallProfile::Compute));
    }

    #[test]
    fn intersect_or_deny_all() {
        let a = SandboxPolicy {
            deny_all: false,
            ..Default::default()
        };
        let b = SandboxPolicy {
            deny_all: true,
            ..Default::default()
        };
        let c = a.intersect(&b);
        assert!(c.deny_all);
    }

    #[test]
    fn intersect_paths_takes_common_only() {
        let a = SandboxPolicy {
            fs: FsPolicy {
                rw: vec![PathBuf::from("/a"), PathBuf::from("/b")],
                ..Default::default()
            },
            ..Default::default()
        };
        let b = SandboxPolicy {
            fs: FsPolicy {
                rw: vec![PathBuf::from("/b"), PathBuf::from("/c")],
                ..Default::default()
            },
            ..Default::default()
        };
        let c = a.intersect(&b);
        assert_eq!(c.fs.rw, vec![PathBuf::from("/b")]);
    }

    #[test]
    fn intersect_unions_deny() {
        let a = SandboxPolicy {
            fs: FsPolicy {
                deny: vec![PathBuf::from("/a")],
                ..Default::default()
            },
            ..Default::default()
        };
        let b = SandboxPolicy {
            fs: FsPolicy {
                deny: vec![PathBuf::from("/b")],
                ..Default::default()
            },
            ..Default::default()
        };
        let c = a.intersect(&b);
        assert_eq!(c.fs.deny.len(), 2);
    }

    #[test]
    fn with_extra_deny_appends() {
        let p = SandboxPolicy::default().with_extra_deny(["/home/user/.ssh"]);
        assert_eq!(p.fs.deny.len(), 1);
    }
}
