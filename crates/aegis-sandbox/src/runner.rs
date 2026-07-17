//! `SandboxRunner` trait, `SandboxCommand` builder, and the platform-agnostic
//! `NoopRunner`.

use crate::error::{SandboxError, SandboxResult};
use crate::policy::SandboxPolicy;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

/// A command to run inside a sandbox.
///
/// This mirrors the subset of `std::process::Command` we actually need, so the
/// caller doesn't have to construct `Command` directly (and so we can attach
/// pre_exec hooks without exposing that unsafe surface).
#[derive(Debug, Clone)]
pub struct SandboxCommand {
    /// Program to execute.
    pub program: OsString,
    /// Command-line arguments (do NOT include argv[0]).
    pub args: Vec<OsString>,
    /// Working directory. Defaults to current process cwd if `None`.
    pub cwd: Option<PathBuf>,
    /// Environment variables. When `None`, inherit parent env.
    /// When `Some`, exactly these are set (with parent env cleared).
    pub env: Option<HashMap<OsString, OsString>>,
    /// Additional env vars to overlay on the parent env (only used when
    /// `env` is `None`).
    pub env_overlay: HashMap<OsString, OsString>,
    /// Stdin/stdout/stderr configuration (defaults: inherited, piped, piped).
    pub stdin: StdioCfg,
    /// See `stdin`.
    pub stdout: StdioCfg,
    /// See `stdin`.
    pub stderr: StdioCfg,
}

/// Stdio configuration for a `SandboxCommand`. Kept simple on purpose: three
/// values map cleanly to `Stdio`.
#[derive(Debug, Clone, Copy, Default)]
pub enum StdioCfg {
    /// Inherit from parent.
    Inherit,
    /// Discard (`/dev/null`).
    Null,
    /// Capture (default for stdout/stderr).
    #[default]
    Piped,
}

impl From<StdioCfg> for Stdio {
    fn from(c: StdioCfg) -> Self {
        match c {
            StdioCfg::Inherit => Stdio::inherit(),
            StdioCfg::Null => Stdio::null(),
            StdioCfg::Piped => Stdio::piped(),
        }
    }
}

impl SandboxCommand {
    /// Create a new command with the given program and no args.
    pub fn new<S: Into<OsString>>(program: S) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: None,
            env: None,
            env_overlay: HashMap::new(),
            stdin: StdioCfg::Null,
            stdout: StdioCfg::Piped,
            stderr: StdioCfg::Piped,
        }
    }

    /// Append an argument.
    #[must_use]
    pub fn arg<S: Into<OsString>>(mut self, a: S) -> Self {
        self.args.push(a.into());
        self
    }

    /// Append multiple arguments.
    #[must_use]
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Set the working directory.
    #[must_use]
    pub fn cwd<P: Into<PathBuf>>(mut self, p: P) -> Self {
        self.cwd = Some(p.into());
        self
    }

    /// Set an env var (overlaid on parent env).
    #[must_use]
    pub fn env<K: Into<OsString>, V: Into<OsString>>(mut self, key: K, val: V) -> Self {
        self.env_overlay.insert(key.into(), val.into());
        self
    }

    /// Build a `std::process::Command` reflecting this command (before any
    /// pre_exec hook is attached).
    pub(crate) fn to_command(&self) -> Command {
        let mut c = Command::new(&self.program);
        for a in &self.args {
            c.arg(a);
        }
        if let Some(cwd) = &self.cwd {
            c.current_dir(cwd);
        }
        if let Some(env) = &self.env {
            c.env_clear();
            for (k, v) in env {
                c.env(k, v);
            }
        } else {
            for (k, v) in &self.env_overlay {
                c.env(k, v);
            }
        }
        c.stdin(Stdio::from(self.stdin))
            .stdout(Stdio::from(self.stdout))
            .stderr(Stdio::from(self.stderr));
        c
    }

    /// Return the program name as a lossy string (for logging).
    pub fn program_display(&self) -> String {
        self.program.to_string_lossy().into_owned()
    }
}

/// A spawned sandboxed child.
///
/// Wraps `std::process::Child` so we can attach sandbox-specific teardown
/// (like a wall-clock watchdog) in the future without breaking the API.
pub struct SandboxChild {
    inner: Child,
}

impl SandboxChild {
    pub(crate) fn new(inner: Child) -> Self {
        Self { inner }
    }

    /// The child's process id.
    pub fn id(&self) -> u32 {
        self.inner.id()
    }

    /// Consume the child and return the underlying `std::process::Child`.
    /// Callers using `tokio::process` should typically not call this; instead
    /// use the pre_exec-aware wrapper (Phase 2).
    pub fn into_inner(self) -> Child {
        self.inner
    }

    /// Take stdout for reading.
    pub fn take_stdout(&mut self) -> Option<std::process::ChildStdout> {
        self.inner.stdout.take()
    }

    /// Take stderr for reading.
    pub fn take_stderr(&mut self) -> Option<std::process::ChildStderr> {
        self.inner.stderr.take()
    }

    /// Wait for the child to exit and collect its output.
    pub fn wait_with_output(self) -> std::io::Result<std::process::Output> {
        self.inner.wait_with_output()
    }
}

/// Describes what a runner can actually enforce on this platform.
///
/// Used by `spawn` to decide whether to error (when `allow_degrade=false`)
/// or fall back with a warning (when `allow_degrade=true`).
#[derive(Debug, Clone, Copy, Default)]
pub struct RunnerCapabilities {
    /// Can enforce landlock filesystem restrictions.
    pub landlock: bool,
    /// Can install seccomp filters.
    pub seccomp: bool,
    /// Can create user namespaces (uid mapping).
    pub user_ns: bool,
    /// Can create network namespaces.
    pub network_ns: bool,
    /// Can apply rlimits.
    pub rlimit: bool,
}

/// A backend that can execute a `SandboxCommand` under a `SandboxPolicy`.
///
/// Implementations MUST honor `policy.deny_all` by returning an error and
/// never spawning the child.
pub trait SandboxRunner: Send + Sync {
    /// Runner name for logging (e.g. `"linux"`, `"noop"`).
    fn name(&self) -> &'static str;

    /// What this runner can enforce.
    fn capabilities(&self) -> RunnerCapabilities;

    /// Spawn the command under the policy.
    fn spawn(&self, cmd: SandboxCommand, policy: &SandboxPolicy) -> SandboxResult<SandboxChild>;
}

/// A no-op runner: executes the command as-is, ignoring the policy.
///
/// Used as the fallback when the platform doesn't support any real
/// sandboxing, and as the "sandbox disabled" runner when
/// `[sandbox] enabled = false`.
///
/// Still honors `policy.deny_all` — that flag is enforced by the runner
/// contract, not by kernel primitives.
#[derive(Debug, Default)]
pub struct NoopRunner;

impl NoopRunner {
    /// Create a new noop runner.
    pub fn new() -> Self {
        Self
    }
}

impl SandboxRunner for NoopRunner {
    fn name(&self) -> &'static str {
        "noop"
    }

    fn capabilities(&self) -> RunnerCapabilities {
        RunnerCapabilities::default()
    }

    fn spawn(&self, cmd: SandboxCommand, policy: &SandboxPolicy) -> SandboxResult<SandboxChild> {
        if policy.deny_all {
            return Err(SandboxError::InvalidPolicy(
                "policy.deny_all is set; refusing to spawn".into(),
            ));
        }
        let mut command = cmd.to_command();
        let child = command
            .spawn()
            .map_err(|e| SandboxError::Spawn(std::io::Error::new(e.kind(), e.to_string())))?;
        Ok(SandboxChild::new(child))
    }
}

/// Helper for tests: assert deny_all behavior.
impl SandboxCommand {
    #[doc(hidden)]
    pub fn __program_for_test(&self) -> &OsStr {
        &self.program
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_runs_command() {
        let cmd = SandboxCommand::new("true");
        let r = NoopRunner::new();
        let child = r.spawn(cmd, &SandboxPolicy::default()).expect("spawn true");
        let out = child.wait_with_output().expect("wait");
        assert!(out.status.success());
    }

    #[test]
    fn noop_honors_deny_all() {
        let cmd = SandboxCommand::new("true");
        let r = NoopRunner::new();
        let policy = SandboxPolicy {
            deny_all: true,
            ..Default::default()
        };
        let err = r.spawn(cmd, &policy).unwrap_err();
        assert!(matches!(err, SandboxError::InvalidPolicy(_)));
    }

    #[test]
    fn command_builder_captures_args_and_env() {
        let cmd = SandboxCommand::new("sh")
            .arg("-c")
            .arg("echo $FOO")
            .env("FOO", "bar");
        assert_eq!(cmd.args.len(), 2);
        assert_eq!(cmd.env_overlay.len(), 1);
    }
}
