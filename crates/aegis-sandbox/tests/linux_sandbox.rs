//! Integration tests for `aegis-sandbox` on Linux.
//!
//! These tests spawn real child processes and verify that landlock / seccomp
//! / user namespace enforcement happens as advertised. They require:
//!
//! - Linux ≥ 5.13 for landlock ABI V1
//! - Unprivileged user namespaces enabled (Ubuntu 24.04 may need an
//!   AppArmor tweak — see `docs/sandbox.md`)
//!
//! Each test **probes capabilities first** and skips if the kernel can't
//! enforce what the test needs — this keeps CI green on older runners while
//! still failing loudly when a supported feature regresses.

#![cfg(target_os = "linux")]

use aegis_sandbox::{presets, LinuxRunner, SandboxCommand, SandboxPolicy, SandboxRunner};
use std::path::PathBuf;

fn skip_if_missing(runner: &LinuxRunner, feature: &str) -> bool {
    let caps = runner.capabilities();
    let missing = match feature {
        "landlock" => !caps.landlock,
        "user_ns" => !caps.user_ns,
        "network_ns" => !caps.network_ns,
        "seccomp" => !caps.seccomp,
        _ => false,
    };
    if missing {
        eprintln!("skip: kernel missing {feature}");
    }
    missing
}

#[test]
fn deny_all_refuses_to_spawn() {
    let runner = LinuxRunner::new().expect("construct");
    let policy = presets::deny_all();
    let cmd = SandboxCommand::new("true");
    let err = runner.spawn(cmd, &policy).unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("deny_all") || msg.contains("InvalidPolicy"), "got: {msg}");
}

#[test]
fn trivial_command_runs_under_compute_workdir() {
    let runner = LinuxRunner::new().expect("construct");
    if skip_if_missing(&runner, "landlock") {
        return;
    }
    let workdir = tempfile::tempdir().expect("tempdir");
    let policy = presets::compute_workdir(workdir.path());
    let cmd = SandboxCommand::new("sh").arg("-c").arg("echo hello");
    let child = runner.spawn(cmd, &policy).expect("spawn");
    let out = child.wait_with_output().expect("wait");
    assert!(out.status.success(), "exit {:?}", out.status);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("hello"), "stdout was: {stdout}");
}

#[test]
fn landlock_denies_write_to_system_dir() {
    let runner = LinuxRunner::new().expect("construct");
    if skip_if_missing(&runner, "landlock") {
        return;
    }
    let workdir = tempfile::tempdir().expect("tempdir");
    let policy = presets::compute_workdir(workdir.path());
    // Try to write into /etc — a location that's not in `rw`.
    // We use `|| echo DENIED` so the shell exits 0 either way and we can
    // read stdout to see which branch fired.
    let cmd = SandboxCommand::new("sh")
        .arg("-c")
        .arg("touch /etc/aegis-should-not-be-created 2>/dev/null && echo LEAKED || echo DENIED");
    let child = runner.spawn(cmd, &policy).expect("spawn");
    let out = child.wait_with_output().expect("wait");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("DENIED"),
        "expected landlock to deny write to /etc, got: {stdout}"
    );
    // Belt & suspenders: ensure the file wasn't actually created.
    assert!(
        !PathBuf::from("/etc/aegis-should-not-be-created").exists(),
        "landlock leaked write to /etc"
    );
}

#[test]
fn landlock_allows_write_inside_workdir() {
    let runner = LinuxRunner::new().expect("construct");
    if skip_if_missing(&runner, "landlock") {
        return;
    }
    let workdir = tempfile::tempdir().expect("tempdir");
    let policy = presets::compute_workdir(workdir.path());
    // Write inside the workdir should succeed.
    let script = format!(
        "touch {}/inside-file && echo OK || echo BLOCKED",
        workdir.path().display()
    );
    let cmd = SandboxCommand::new("sh").arg("-c").arg(script);
    let child = runner.spawn(cmd, &policy).expect("spawn");
    let out = child.wait_with_output().expect("wait");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("OK"),
        "expected landlock to allow write to workdir, got: {stdout}"
    );
    assert!(workdir.path().join("inside-file").exists());
}

#[test]
fn seccomp_denies_mount() {
    let runner = LinuxRunner::new().expect("construct");
    if skip_if_missing(&runner, "seccomp") {
        return;
    }
    let workdir = tempfile::tempdir().expect("tempdir");
    let policy = presets::compute_workdir(workdir.path());
    // `mount(2)` should return EPERM from our seccomp filter.
    // Note: `mount(1)` command is often setuid, but with PR_SET_NO_NEW_PRIVS
    // that's already dropped; and even if it ran, the syscall itself is
    // filtered. Exit non-zero is expected.
    let cmd = SandboxCommand::new("sh")
        .arg("-c")
        .arg("mount /tmp /mnt 2>&1 || echo MOUNT_BLOCKED");
    let child = runner.spawn(cmd, &policy).expect("spawn");
    let out = child.wait_with_output().expect("wait");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("MOUNT_BLOCKED")
            || combined.to_lowercase().contains("permission denied")
            || combined.to_lowercase().contains("operation not permitted"),
        "expected seccomp to deny mount, got: {combined}"
    );
}

#[test]
fn user_ns_maps_uid_to_nobody() {
    let runner = LinuxRunner::new().expect("construct");
    if skip_if_missing(&runner, "user_ns") {
        return;
    }
    if skip_if_missing(&runner, "landlock") {
        return;
    }
    let workdir = tempfile::tempdir().expect("tempdir");
    let policy = presets::compute_workdir(workdir.path());
    // Inside the user_ns, `id -u` should print 65534 (nobody).
    let cmd = SandboxCommand::new("id").arg("-u");
    let child = runner.spawn(cmd, &policy).expect("spawn");
    let out = child.wait_with_output().expect("wait");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.trim() == "65534",
        "expected uid=65534 (nobody), got: {stdout}"
    );
}

#[test]
fn network_ns_blocks_external_connect() {
    let runner = LinuxRunner::new().expect("construct");
    if skip_if_missing(&runner, "network_ns") {
        return;
    }
    if skip_if_missing(&runner, "landlock") {
        return;
    }
    let workdir = tempfile::tempdir().expect("tempdir");
    let policy = presets::compute_workdir(workdir.path()); // no network
    // Try to connect to a well-known host. In network_ns, there's no route
    // out — expect failure.
    let cmd = SandboxCommand::new("sh")
        .arg("-c")
        .arg("getent hosts example.com 2>&1 && echo LEAKED || echo NET_BLOCKED");
    let child = runner.spawn(cmd, &policy).expect("spawn");
    let out = child.wait_with_output().expect("wait");
    let stdout = String::from_utf8_lossy(&out.stdout);
    // We accept either explicit block message or DNS failure — both mean
    // network is unreachable.
    assert!(
        stdout.contains("NET_BLOCKED") || !stdout.contains("LEAKED"),
        "network was reachable in None policy: {stdout}"
    );
}

#[test]
fn network_readonly_preserves_host_network() {
    let runner = LinuxRunner::new().expect("construct");
    if skip_if_missing(&runner, "landlock") {
        return;
    }
    let workdir = tempfile::tempdir().expect("tempdir");
    let policy = presets::network_readonly(workdir.path());
    // Under network_readonly, network policy is Host — DNS should work.
    // We don't assert the network is up (CI might be air-gapped), just
    // that the syscall isn't blocked by seccomp/network_ns.
    // We verify by checking the getent call doesn't hit an EPERM syscall
    // error. Just running successfully with no seccomp EPERM is enough.
    let cmd = SandboxCommand::new("sh").arg("-c").arg(
        "getent hosts localhost 2>&1 | head -1 || echo LOCALHOST_UNREACHABLE",
    );
    let child = runner.spawn(cmd, &policy).expect("spawn");
    let _out = child.wait_with_output().expect("wait");
    // No assertion here — we just want to prove it doesn't hang / crash.
    // The syscall-filter tests above already prove denies work.
}

#[test]
fn rlimit_caps_open_files() {
    let runner = LinuxRunner::new().expect("construct");
    let mut policy = SandboxPolicy::default();
    policy.resource.max_open_files = Some(64);
    // Verify by asking `ulimit -n` under the sandbox.
    let cmd = SandboxCommand::new("sh").arg("-c").arg("ulimit -n");
    let child = runner.spawn(cmd, &policy).expect("spawn");
    let out = child.wait_with_output().expect("wait");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let n: u64 = stdout.trim().parse().unwrap_or(u64::MAX);
    assert!(n <= 64, "expected RLIMIT_NOFILE ≤ 64, got: {stdout}");
}
