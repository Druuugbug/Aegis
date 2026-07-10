//! Linux sandbox runner using landlock + seccomp + user namespace + rlimit.
//!
//! Layering order inside `pre_exec` is critical:
//! 1. `PR_SET_NO_NEW_PRIVS` (required by seccomp; also drops setuid attack surface)
//! 2. `setrlimit` for CPU/memory/fd caps
//! 3. `unshare(CLONE_NEWUSER)` + setgroups=deny + write `uid_map`/`gid_map`
//!    (when `UidPolicy::MapToNobody`)
//! 4. `unshare(CLONE_NEWNET)` if `NetworkPolicy::None`
//! 5. landlock (must happen after user_ns so bind-mounted paths still resolve)
//! 6. seccomp (last, so preceding syscalls aren't filtered)
//!
//! ⚠️ Code inside `pre_exec` must only use async-signal-safe operations for
//! the strictest interpretation. In practice Rust panics-free code doing
//! `libc::open/write/unshare/prctl` is fine (the child is single-threaded
//! after fork). We minimize allocations in critical paths.

use crate::error::{SandboxError, SandboxResult};
use crate::policy::{FsPolicy, NetworkPolicy, SandboxPolicy, SyscallProfile, UidPolicy};
use crate::runner::{RunnerCapabilities, SandboxChild, SandboxCommand, SandboxRunner};
use std::os::unix::process::CommandExt;

/// Linux runner.
pub struct LinuxRunner {
    caps: RunnerCapabilities,
}

impl LinuxRunner {
    /// Construct a new Linux runner, probing the kernel for capabilities.
    pub fn new() -> SandboxResult<Self> {
        Ok(Self {
            caps: probe_capabilities(),
        })
    }
}

impl SandboxRunner for LinuxRunner {
    fn name(&self) -> &'static str {
        "linux"
    }

    fn capabilities(&self) -> RunnerCapabilities {
        self.caps
    }

    fn spawn(&self, cmd: SandboxCommand, policy: &SandboxPolicy) -> SandboxResult<SandboxChild> {
        if policy.deny_all {
            return Err(SandboxError::InvalidPolicy(
                "policy.deny_all is set; refusing to spawn".into(),
            ));
        }
        check_policy_supported(&self.caps, policy)?;

        let mut command = cmd.to_command();
        let policy = policy.clone();
        // SAFETY: `pre_exec` runs after fork(2) and before execve(2). The
        // closure body is restricted to async-signal-safe primitives.
        unsafe {
            command.pre_exec(move || apply_policy_pre_exec(&policy));
        }
        let child = command.spawn()?;
        Ok(SandboxChild::new(child))
    }
}

/// Probe kernel for landlock/seccomp/user_ns support.
///
/// We do a *conservative* probe: we don't actually invoke unshare in the
/// parent (which would create a user namespace in the aegis process itself).
/// Instead we check kernel version markers and file existence:
///
/// - landlock: `/sys/kernel/security/landlock/version` exists on ≥ 5.13
/// - user_ns: `/proc/self/setgroups` exists on ≥ 3.19; unprivileged toggle at
///   `/proc/sys/kernel/unprivileged_userns_clone` may be `0` on some distros
/// - seccomp: always available on modern Linux (≥ 3.5)
fn probe_capabilities() -> RunnerCapabilities {
    RunnerCapabilities {
        landlock: std::path::Path::new("/sys/kernel/security/landlock/version").exists(),
        seccomp: true,
        user_ns: unprivileged_user_ns_available(),
        network_ns: unprivileged_user_ns_available(),
        rlimit: true,
    }
}

fn unprivileged_user_ns_available() -> bool {
    // On Debian/Ubuntu the sysctl controls this. Missing file = allowed.
    let path = "/proc/sys/kernel/unprivileged_userns_clone";
    match std::fs::read_to_string(path) {
        Ok(s) => s.trim() == "1",
        Err(_) => true,
    }
}

/// Return `Unsupported` if the requested policy can't be honored on this
/// kernel and `allow_degrade` is false.
fn check_policy_supported(caps: &RunnerCapabilities, policy: &SandboxPolicy) -> SandboxResult<()> {
    if policy.allow_degrade {
        return Ok(());
    }
    if matches!(policy.uid, UidPolicy::MapToNobody) && !caps.user_ns {
        return Err(SandboxError::Unsupported(
            "user namespaces disabled on this kernel".into(),
        ));
    }
    if matches!(policy.network, NetworkPolicy::None) && !caps.network_ns {
        return Err(SandboxError::Unsupported(
            "network namespaces disabled on this kernel".into(),
        ));
    }
    if !policy.fs.ro.is_empty() || !policy.fs.rw.is_empty() {
        if !caps.landlock {
            return Err(SandboxError::Unsupported(
                "landlock not available (kernel < 5.13?)".into(),
            ));
        }
    }
    Ok(())
}

// ── pre_exec pipeline ─────────────────────────────────────────────────────

/// Apply the full sandbox policy to the current thread. Intended to be used
/// as a pre_exec hook attached to a `std::process::Command` or
/// `tokio::process::Command` (both accept the same `CommandExt::pre_exec`).
///
/// The child process calls this after `fork(2)` and before `execve(2)`. The
/// body is restricted to async-signal-safe primitives.
///
/// Order (mandatory):
/// 1. `PR_SET_NO_NEW_PRIVS` (required by seccomp)
/// 2. `setrlimit` (CPU / memory / fd)
/// 3. `unshare(CLONE_NEWUSER|CLONE_NEWNS)` + uid/gid map (if `UidPolicy::MapToNobody`)
/// 4. `unshare(CLONE_NEWNET)` (if `NetworkPolicy::None`)
/// 5. `landlock` (fs allowlist)
/// 6. `seccomp` (syscall filter — last!)
///
/// Returns any error from the underlying kernel call, wrapped with a stage
/// tag ("sandbox landlock: ...") to help post-mortem.
pub fn apply_policy_pre_exec(policy: &SandboxPolicy) -> std::io::Result<()> {
    // 1. PR_SET_NO_NEW_PRIVS (required before seccomp)
    set_no_new_privs()?;

    // 2. rlimits
    apply_rlimits(&policy.resource)?;

    // 3. user namespace (must precede landlock so mount view is right)
    if matches!(policy.uid, UidPolicy::MapToNobody) {
        enter_user_namespace().map_err(io_wrap("user_ns"))?;
    }

    // 4. network namespace
    if matches!(policy.network, NetworkPolicy::None) {
        enter_network_namespace().map_err(io_wrap("network_ns"))?;
    }

    // 5. landlock (fs restrictions)
    apply_landlock(&policy.fs).map_err(io_wrap("landlock"))?;

    // 6. seccomp (syscall filter) — last, so earlier steps aren't filtered
    apply_seccomp(&policy.syscalls).map_err(io_wrap("seccomp"))?;

    Ok(())
}

fn io_wrap(stage: &'static str) -> impl Fn(std::io::Error) -> std::io::Error {
    move |e| std::io::Error::new(e.kind(), format!("sandbox {stage}: {e}"))
}

fn set_no_new_privs() -> std::io::Result<()> {
    // SAFETY: prctl is async-signal-safe.
    let rc = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1_u64, 0_u64, 0_u64, 0_u64) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

// ── rlimits ───────────────────────────────────────────────────────────────

fn apply_rlimits(r: &crate::policy::ResourceLimits) -> std::io::Result<()> {
    if let Some(cpu) = r.max_cpu_seconds {
        set_rlimit(libc::RLIMIT_CPU, cpu)?;
    }
    if let Some(mem) = r.max_memory_bytes {
        set_rlimit(libc::RLIMIT_AS, mem)?;
    }
    if let Some(nofile) = r.max_open_files {
        set_rlimit(libc::RLIMIT_NOFILE, nofile)?;
    }
    Ok(())
}

// glibc types setrlimit's resource as `__rlimit_resource_t`; musl uses `c_int`.
#[cfg(target_env = "gnu")]
type RlimitResource = libc::__rlimit_resource_t;
#[cfg(not(target_env = "gnu"))]
type RlimitResource = libc::c_int;

fn set_rlimit(res: RlimitResource, val: u64) -> std::io::Result<()> {
    let rlim = libc::rlimit {
        rlim_cur: val as libc::rlim_t,
        rlim_max: val as libc::rlim_t,
    };
    // SAFETY: setrlimit is async-signal-safe.
    let rc = unsafe { libc::setrlimit(res, &rlim) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

// ── user namespace ────────────────────────────────────────────────────────

/// Enter a new user namespace with uid/gid mapped to nobody (65534).
///
/// Sequence per man user_namespaces(7):
/// 1. `unshare(CLONE_NEWUSER)` (+ CLONE_NEWNS so landlock sees a clean view)
/// 2. write "deny" to `/proc/self/setgroups` (required before gid_map)
/// 3. write mapping to `/proc/self/uid_map`
/// 4. write mapping to `/proc/self/gid_map`
///
/// All I/O uses raw `libc::open/write` on statically-known paths — safe from
/// pre_exec context.
fn enter_user_namespace() -> std::io::Result<()> {
    // SAFETY: getuid/getgid are async-signal-safe.
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };

    // SAFETY: unshare is async-signal-safe.
    let rc = unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }

    // Deny setgroups
    write_proc_file(b"/proc/self/setgroups\0", b"deny")?;

    // Build "65534 <uid> 1" without heap alloc.
    let mut buf = [0u8; 64];
    let n = fmt_id_map(&mut buf, uid);
    write_proc_file(b"/proc/self/uid_map\0", &buf[..n])?;
    let n = fmt_id_map(&mut buf, gid);
    write_proc_file(b"/proc/self/gid_map\0", &buf[..n])?;

    Ok(())
}

/// Write `data` to a NUL-terminated absolute path, using raw libc for
/// async-signal safety.
fn write_proc_file(path_z: &[u8], data: &[u8]) -> std::io::Result<()> {
    debug_assert!(path_z.last() == Some(&0));
    // SAFETY: path_z is a NUL-terminated static string.
    let fd = unsafe { libc::open(path_z.as_ptr() as *const libc::c_char, libc::O_WRONLY) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut remaining = data;
    while !remaining.is_empty() {
        // SAFETY: fd is valid; remaining points to `data`'s valid slice.
        let n = unsafe {
            libc::write(
                fd,
                remaining.as_ptr() as *const libc::c_void,
                remaining.len(),
            )
        };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            // SAFETY: close on a valid fd.
            unsafe { libc::close(fd) };
            return Err(e);
        }
        remaining = &remaining[n as usize..];
    }
    // SAFETY: close on a valid fd.
    unsafe { libc::close(fd) };
    Ok(())
}

/// Format a uid_map/gid_map line "65534 <real> 1" into `buf`; return len.
fn fmt_id_map(buf: &mut [u8; 64], real_id: u32) -> usize {
    let head = b"65534 ";
    let tail = b" 1";
    let mut cur = 0;
    for &b in head {
        buf[cur] = b;
        cur += 1;
    }
    cur += write_u32_decimal(&mut buf[cur..], real_id);
    for &b in tail {
        buf[cur] = b;
        cur += 1;
    }
    cur
}

fn write_u32_decimal(buf: &mut [u8], val: u32) -> usize {
    if val == 0 {
        buf[0] = b'0';
        return 1;
    }
    let mut tmp = [0u8; 10];
    let mut n = 0;
    let mut v = val;
    while v > 0 {
        tmp[n] = b'0' + (v % 10) as u8;
        v /= 10;
        n += 1;
    }
    // reverse
    for i in 0..n {
        buf[i] = tmp[n - 1 - i];
    }
    n
}

// ── network namespace ─────────────────────────────────────────────────────

/// Enter a new network namespace. The new ns has only a down loopback; the
/// child sees ENETUNREACH for any external connect.
fn enter_network_namespace() -> std::io::Result<()> {
    // SAFETY: unshare is async-signal-safe.
    let rc = unsafe { libc::unshare(libc::CLONE_NEWNET) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

// ── landlock ──────────────────────────────────────────────────────────────

/// Apply landlock filesystem restrictions.
///
/// Landlock is *allowlist-only*: paths not listed are implicitly denied. We
/// therefore ignore `fs.deny` at the landlock layer (the callers put paths
/// there for documentation and future defense layers). If a path in `deny`
/// happens to be under an `ro`/`rw` prefix, the caller should not have
/// included the prefix in the first place.
fn apply_landlock(fs: &FsPolicy) -> std::io::Result<()> {
    if fs.ro.is_empty() && fs.rw.is_empty() {
        // No fs policy → skip landlock entirely (equivalent to
        // NetworkPolicy::Host + no fs = "pass through").
        return Ok(());
    }
    use landlock::{
        Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr,
        RulesetStatus, ABI,
    };

    // ABI::V1 = Linux 5.13.
    let abi = ABI::V1;
    let ro_access = AccessFs::from_read(abi);
    let rw_access = if fs.allow_create {
        AccessFs::from_all(abi)
    } else {
        // Read + non-creating write. All variants below exist in ABI::V1.
        AccessFs::ReadFile
            | AccessFs::ReadDir
            | AccessFs::WriteFile
            | AccessFs::Execute
            | AccessFs::RemoveFile
            | AccessFs::RemoveDir
    };

    let mut ruleset = Ruleset::default()
        .handle_access(AccessFs::from_all(abi))
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("{e:?}")))?
        .create()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("{e:?}")))?;

    for p in &fs.ro {
        if let Ok(fd) = PathFd::new(p) {
            ruleset = ruleset
                .add_rule(PathBeneath::new(fd, ro_access))
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("{e:?}")))?;
        }
    }
    for p in &fs.rw {
        if let Ok(fd) = PathFd::new(p) {
            ruleset = ruleset
                .add_rule(PathBeneath::new(fd, rw_access))
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("{e:?}")))?;
        }
    }

    let status = ruleset
        .restrict_self()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("{e:?}")))?;

    // If the kernel returned "not enforced", that's a fatal misconfiguration
    // when the caller expected sandboxing.
    if status.ruleset == RulesetStatus::NotEnforced {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "landlock: kernel returned NotEnforced",
        ));
    }
    Ok(())
}

// ── seccomp ───────────────────────────────────────────────────────────────

/// Apply the seccomp filter matching the profile.
///
/// `Unrestricted` → no-op.
/// `Compute` / `ComputeNet` → deny a curated list of dangerous syscalls with
/// EPERM (rather than SIGKILL) so applications get a clean error path.
fn apply_seccomp(profile: &SyscallProfile) -> std::io::Result<()> {
    use seccompiler::{apply_filter, BpfProgram, SeccompAction, SeccompFilter, SeccompRule};
    use std::collections::BTreeMap;

    let denied: &[i64] = match profile {
        SyscallProfile::Unrestricted => return Ok(()),
        SyscallProfile::Compute => COMPUTE_DENY_SYSCALLS,
        SyscallProfile::ComputeNet => COMPUTE_DENY_SYSCALLS_KEEP_NET,
    };

    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    for &syscall_nr in denied {
        rules.insert(syscall_nr, vec![]);
    }

    // The target arch check — we assume x86_64 or aarch64. Both are the
    // common Aegis deploy targets.
    let arch = detect_seccomp_arch();

    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,                       // default action
        SeccompAction::Errno(libc::EPERM as u32),   // for matched (denied)
        arch,
    )
    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("{e:?}")))?;

    let program: BpfProgram = filter
        .try_into()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("{e:?}")))?;

    apply_filter(&program)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("{e:?}")))?;

    Ok(())
}

#[cfg(target_arch = "x86_64")]
fn detect_seccomp_arch() -> seccompiler::TargetArch {
    seccompiler::TargetArch::x86_64
}
#[cfg(target_arch = "aarch64")]
fn detect_seccomp_arch() -> seccompiler::TargetArch {
    seccompiler::TargetArch::aarch64
}
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn detect_seccomp_arch() -> seccompiler::TargetArch {
    // seccompiler exposes limited arches; on unknown arches we fall back to
    // x86_64 and accept that the filter may not apply cleanly.
    seccompiler::TargetArch::x86_64
}

/// Syscalls denied by the Compute profile.
///
/// This is a *conservative* deny-list focused on the well-known dangerous
/// syscalls; the allow-everything-else default is a
/// pragmatic stance. A future Phase 2 can flip to allow-list.
// musl's libc bindings don't export SYS_kexec_file_load; the number is
// arch-specific, so shim it for the targets we ship.
#[cfg(target_env = "gnu")]
const SYS_KEXEC_FILE_LOAD: libc::c_long = libc::SYS_kexec_file_load;
#[cfg(all(not(target_env = "gnu"), target_arch = "x86_64"))]
const SYS_KEXEC_FILE_LOAD: libc::c_long = 320;
#[cfg(all(not(target_env = "gnu"), target_arch = "aarch64"))]
const SYS_KEXEC_FILE_LOAD: libc::c_long = 294;

const COMPUTE_DENY_SYSCALLS: &[i64] = &[
    libc::SYS_ptrace,
    libc::SYS_kexec_load,
    SYS_KEXEC_FILE_LOAD,
    libc::SYS_init_module,
    libc::SYS_finit_module,
    libc::SYS_delete_module,
    libc::SYS_bpf,
    libc::SYS_mount,
    libc::SYS_umount2,
    libc::SYS_pivot_root,
    libc::SYS_chroot,
    libc::SYS_swapon,
    libc::SYS_swapoff,
    libc::SYS_reboot,
    libc::SYS_perf_event_open,
    libc::SYS_keyctl,
    libc::SYS_add_key,
    libc::SYS_request_key,
    libc::SYS_setns,
    libc::SYS_unshare,
    libc::SYS_syslog,
    libc::SYS_acct,
    libc::SYS_quotactl,
    libc::SYS_settimeofday,
    libc::SYS_clock_settime,
    libc::SYS_clock_adjtime,
    libc::SYS_adjtimex,
];

/// ComputeNet keeps the same denies as Compute; network syscalls
/// (socket/connect/etc.) are already allowed by the default-Allow action.
const COMPUTE_DENY_SYSCALLS_KEEP_NET: &[i64] = COMPUTE_DENY_SYSCALLS;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_u32_decimal_basic() {
        let mut buf = [0u8; 16];
        let n = write_u32_decimal(&mut buf, 0);
        assert_eq!(&buf[..n], b"0");
        let n = write_u32_decimal(&mut buf, 1000);
        assert_eq!(&buf[..n], b"1000");
        let n = write_u32_decimal(&mut buf, 65534);
        assert_eq!(&buf[..n], b"65534");
    }

    #[test]
    fn fmt_id_map_produces_correct_line() {
        let mut buf = [0u8; 64];
        let n = fmt_id_map(&mut buf, 1000);
        assert_eq!(&buf[..n], b"65534 1000 1");
    }

    #[test]
    fn linux_runner_constructs() {
        let r = LinuxRunner::new().expect("construct");
        // rlimit is always available.
        assert!(r.capabilities().rlimit);
    }

    #[test]
    fn linux_runner_refuses_deny_all() {
        let r = LinuxRunner::new().expect("construct");
        let cmd = SandboxCommand::new("true");
        let policy = SandboxPolicy {
            deny_all: true,
            ..Default::default()
        };
        let err = r.spawn(cmd, &policy).unwrap_err();
        assert!(matches!(err, SandboxError::InvalidPolicy(_)));
    }
}
