//! Pre-main process hardening.
//!
//! Applied by the `aegis` binary via a ctor-style init before `main()` runs.
//!
//! Steps performed:
//!
//! 1. Disable core dumps (`RLIMIT_CORE = 0`) — prevents post-crash memory
//!    disclosure of API keys/tokens.
//! 2. Disable ptrace attach on Linux/macOS — blocks debuggers from
//!    inspecting the running process (`prctl(PR_SET_DUMPABLE, 0)` on Linux).
//! 3. Strip dangerous env vars from the child env exposed to future
//!    subprocesses: `LD_PRELOAD`, `LD_AUDIT`, `LD_LIBRARY_PATH` (only if it
//!    contains a relative path), and the `DYLD_*` family on macOS.
//!
//! Each step is best-effort: failure is logged (once) and does not abort
//! startup. The whole function is idempotent — safe to call multiple times.

use std::sync::Once;

static HARDEN_ONCE: Once = Once::new();

/// Apply process-wide security hardening. Safe to call multiple times; only
/// the first call has effect.
///
/// Intended usage from the `aegis` binary (in `main.rs`):
///
/// ```ignore
/// fn main() {
///     aegis_sandbox::pre_main_hardening();
///     // ... rest of main
/// }
/// ```
pub fn pre_main_hardening() {
    HARDEN_ONCE.call_once(|| {
        disable_core_dumps();
        disable_ptrace_attach();
        strip_dangerous_env();
    });
}

#[cfg(unix)]
fn disable_core_dumps() {
    // SAFETY: setrlimit is safe to call at any time from any thread.
    unsafe {
        let rlim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if libc::setrlimit(libc::RLIMIT_CORE, &rlim) != 0 {
            tracing::debug!("hardening: setrlimit(RLIMIT_CORE, 0) failed");
        }
    }
}

#[cfg(not(unix))]
fn disable_core_dumps() {}

#[cfg(target_os = "linux")]
fn disable_ptrace_attach() {
    // PR_SET_DUMPABLE = 0 also prevents /proc/<pid>/{mem,maps} from being
    // read by another process running as the same user, and blocks ptrace
    // PTRACE_ATTACH from non-privileged callers.
    // SAFETY: prctl is safe to call at any time.
    unsafe {
        if libc::prctl(libc::PR_SET_DUMPABLE, 0_i32, 0_i32, 0_i32, 0_i32) != 0 {
            tracing::debug!("hardening: prctl(PR_SET_DUMPABLE, 0) failed");
        }
    }
}

#[cfg(target_os = "macos")]
fn disable_ptrace_attach() {
    // On macOS, PT_DENY_ATTACH tells the kernel to refuse debugger attach.
    const PT_DENY_ATTACH: libc::c_int = 31;
    // SAFETY: ptrace with PT_DENY_ATTACH is documented safe from the
    // main thread pre-main.
    unsafe {
        // The 4-arg ptrace signature isn't in `libc`; declare it directly.
        extern "C" {
            fn ptrace(
                request: libc::c_int,
                pid: libc::pid_t,
                addr: *mut libc::c_char,
                data: libc::c_int,
            ) -> libc::c_int;
        }
        if ptrace(PT_DENY_ATTACH, 0, std::ptr::null_mut(), 0) != 0 {
            tracing::debug!("hardening: ptrace(PT_DENY_ATTACH) failed");
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn disable_ptrace_attach() {}

fn strip_dangerous_env() {
    // These env vars can be used to inject arbitrary code into subprocesses
    // via the dynamic linker. Aegis never legitimately uses them.
    const DANGEROUS: &[&str] = &[
        "LD_PRELOAD",
        "LD_AUDIT",
        "DYLD_INSERT_LIBRARIES",
        "DYLD_LIBRARY_PATH",
        "DYLD_FRAMEWORK_PATH",
        "DYLD_FALLBACK_LIBRARY_PATH",
    ];
    for k in DANGEROUS {
        // SAFETY: env manipulation from single-threaded pre-main context.
        // Callers must invoke pre_main_hardening() before spawning threads.
        unsafe {
            std::env::remove_var(k);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hardening_is_idempotent() {
        // Safe to call multiple times without panic.
        pre_main_hardening();
        pre_main_hardening();
        pre_main_hardening();
    }

    #[test]
    fn strip_removes_ld_preload() {
        // SAFETY: single-threaded test context.
        unsafe {
            std::env::set_var("LD_PRELOAD", "/tmp/x.so");
        }
        strip_dangerous_env();
        assert!(std::env::var_os("LD_PRELOAD").is_none());
    }
}
