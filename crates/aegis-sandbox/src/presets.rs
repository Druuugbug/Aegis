//! Named sandbox policy presets.
//!
//! Presets are the canonical way for callers to describe *intent* without
//! having to hand-assemble a [`SandboxPolicy`] from primitives. Each preset
//! returns a policy that's safe to intersect with token-declared limits.
//!
//! See `devdocs/design-sandbox.md` for when to use which.

use crate::policy::{
    FsPolicy, NetworkPolicy, ResourceLimits, SandboxPolicy, SyscallProfile, UidPolicy,
};
use std::path::{Path, PathBuf};

/// Standard read-only system paths every preset (except `unrestricted` and
/// `deny_all`) inherits, so programs can load their standard libraries and
/// resolve DNS.
fn standard_ro_paths() -> Vec<PathBuf> {
    vec![
        PathBuf::from("/usr"),
        PathBuf::from("/bin"),
        PathBuf::from("/sbin"),
        PathBuf::from("/lib"),
        PathBuf::from("/lib64"),
        PathBuf::from("/etc/ssl"),
        PathBuf::from("/etc/resolv.conf"),
        PathBuf::from("/etc/nsswitch.conf"),
        PathBuf::from("/etc/hosts"),
    ]
}

/// Paths that should always be denied regardless of preset, for defense in
/// depth. Note that under the standard preset baselines these paths aren't in
/// `ro` either, so they'd already be inaccessible under landlock — this list
/// is a belt-and-suspenders explicit-deny.
fn always_deny_paths() -> Vec<PathBuf> {
    let mut v = vec![PathBuf::from("/root"), PathBuf::from("/var/log/auth.log")];
    if let Some(h) = dirs_next_home() {
        v.push(h.join(".ssh"));
        v.push(h.join(".aws"));
        v.push(h.join(".gnupg"));
        v.push(h.join(".config").join("aegis"));
        v.push(h.join(".aegis"));
    }
    v
}

/// Minimal wrapper around `std::env` to look up the caller's home dir
/// without pulling `dirs_next` into this crate. Callers that want a
/// specific home dir should override via `with_extra_deny`.
fn dirs_next_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Group of factory functions returning ready-to-use [`SandboxPolicy`] values.
///
/// (These live at the module root — earlier revisions wrapped them in an
/// inner `pub mod presets {}` which collided with the outer `mod presets`
/// declaration in `lib.rs`.)

/// No restrictions at all. Equivalent to running without sandbox.
///
/// Only appropriate for [`Identity::LocalOwner`]-style trust levels.
///
/// [`Identity::LocalOwner`]: <see `aegis_security::identity`>
pub fn unrestricted() -> SandboxPolicy {
    SandboxPolicy::default()
}

/// Deny everything — the runner will refuse to spawn.
///
/// Used for `TrustLevel::ReadOnly` when a caller tries to invoke a
/// spawning tool: rejection happens at policy-derivation time.
pub fn deny_all() -> SandboxPolicy {
    SandboxPolicy {
        deny_all: true,
        ..Default::default()
    }
}

/// Compute + workdir R/W. No network. Uid mapped to nobody.
///
/// Use for: sub-agent code execution, sandboxed `terminal` for
/// non-Owner identities, `spawn_task` isolation, `aegis evolve` builds.
pub fn compute_workdir(workdir: &Path) -> SandboxPolicy {
    let mut ro = standard_ro_paths();
    ro.push(workdir.to_path_buf());
    SandboxPolicy {
        fs: FsPolicy {
            ro,
            rw: vec![workdir.to_path_buf(), PathBuf::from("/tmp")],
            deny: always_deny_paths(),
            allow_create: true,
        },
        syscalls: SyscallProfile::Compute,
        network: NetworkPolicy::None,
        uid: UidPolicy::MapToNobody,
        resource: ResourceLimits {
            max_cpu_seconds: Some(300),
            max_memory_bytes: Some(1_073_741_824), // 1 GiB
            max_open_files: Some(1024),
            max_wall_seconds: Some(600),
        },
        allow_degrade: false,
        deny_all: false,
    }
}

/// Whole disk read-only, `/tmp` writable, host network available.
///
/// Use for: `web_extract` (fetches URLs), `browser` harness, anything
/// that reads from the network and produces output only in `/tmp`.
pub fn network_readonly(workdir: &Path) -> SandboxPolicy {
    let mut ro = standard_ro_paths();
    ro.push(workdir.to_path_buf());
    SandboxPolicy {
        fs: FsPolicy {
            ro,
            rw: vec![PathBuf::from("/tmp")],
            deny: always_deny_paths(),
            allow_create: true,
        },
        syscalls: SyscallProfile::ComputeNet,
        network: NetworkPolicy::Host,
        uid: UidPolicy::MapToNobody,
        resource: ResourceLimits {
            max_cpu_seconds: Some(120),
            max_memory_bytes: Some(536_870_912), // 512 MiB
            max_open_files: Some(1024),
            max_wall_seconds: Some(180),
        },
        allow_degrade: false,
        deny_all: false,
    }
}

/// Read a specific list of input files, no write, no network.
///
/// Use for: parsing untrusted content (HTML/JSON/PDF/etc.) where
/// the parser must not phone home nor persist state.
pub fn parser_offline(inputs: &[PathBuf]) -> SandboxPolicy {
    let mut ro = standard_ro_paths();
    ro.extend(inputs.iter().cloned());
    SandboxPolicy {
        fs: FsPolicy {
            ro,
            rw: Vec::new(),
            deny: always_deny_paths(),
            allow_create: false,
        },
        syscalls: SyscallProfile::Compute,
        network: NetworkPolicy::None,
        uid: UidPolicy::MapToNobody,
        resource: ResourceLimits {
            max_cpu_seconds: Some(60),
            max_memory_bytes: Some(268_435_456), // 256 MiB
            max_open_files: Some(256),
            max_wall_seconds: Some(90),
        },
        allow_degrade: false,
        deny_all: false,
    }
}

/// Preset for MCP server subprocesses.
///
/// Read+write to the MCP server's config directory + `/tmp`, network on
/// (MCP servers commonly call external APIs). Uid mapped to nobody.
pub fn mcp_server() -> SandboxPolicy {
    let mut ro = standard_ro_paths();
    let mut rw = vec![PathBuf::from("/tmp")];
    if let Some(h) = dirs_next_home() {
        let mcp_dir = h.join(".config").join("aegis").join("mcp");
        ro.push(mcp_dir.clone());
        rw.push(mcp_dir);
    }
    SandboxPolicy {
        fs: FsPolicy {
            ro,
            rw,
            deny: always_deny_paths(),
            allow_create: true,
        },
        syscalls: SyscallProfile::ComputeNet,
        network: NetworkPolicy::Host,
        uid: UidPolicy::MapToNobody,
        resource: ResourceLimits {
            max_cpu_seconds: Some(600),
            max_memory_bytes: Some(536_870_912), // 512 MiB
            max_open_files: Some(1024),
            max_wall_seconds: Some(1800),
        },
        allow_degrade: true,
        deny_all: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{NetworkPolicy, SyscallProfile, UidPolicy};
    use std::path::Path;

    #[test]
    fn unrestricted_is_permissive() {
        let p = unrestricted();
        assert!(matches!(p.syscalls, SyscallProfile::Unrestricted));
        assert!(matches!(p.network, NetworkPolicy::Host));
        assert!(matches!(p.uid, UidPolicy::Keep));
        assert!(!p.deny_all);
    }

    #[test]
    fn deny_all_denies() {
        let p = deny_all();
        assert!(p.deny_all);
    }

    #[test]
    fn compute_workdir_no_network() {
        let p = compute_workdir(Path::new("/home/u/work"));
        assert!(matches!(p.network, NetworkPolicy::None));
        assert!(matches!(p.syscalls, SyscallProfile::Compute));
        assert!(matches!(p.uid, UidPolicy::MapToNobody));
    }

    #[test]
    fn network_readonly_has_network_no_workdir_write() {
        let p = network_readonly(Path::new("/home/u/work"));
        assert!(matches!(p.network, NetworkPolicy::Host));
        // rw should only contain /tmp, not the workdir
        assert_eq!(p.fs.rw.len(), 1);
        assert_eq!(p.fs.rw[0], std::path::PathBuf::from("/tmp"));
    }

    #[test]
    fn parser_offline_reads_inputs_only() {
        let p = parser_offline(&[std::path::PathBuf::from("/tmp/input.html")]);
        assert!(p.fs.rw.is_empty());
        assert!(!p.fs.allow_create);
        assert!(matches!(p.network, NetworkPolicy::None));
    }

    #[test]
    fn mcp_server_allows_degradation() {
        let p = mcp_server();
        assert!(p.allow_degrade);
        assert!(matches!(p.network, NetworkPolicy::Host));
    }
}
