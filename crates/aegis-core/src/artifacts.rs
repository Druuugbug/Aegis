//! Artifact manifest — the single authoritative registry of everything Aegis
//! writes to the filesystem.
//!
//! Motivation: Aegis's on-disk products were previously defined ad-hoc as
//! `config_dir().join(...)` calls scattered across 13+ files, with no single
//! place that knew "all of Aegis's artifacts". `backup` had a hardcoded
//! `CURATED` list that silently missed newly-added products; `uninstall` could
//! only see what lives under the config dir. This module centralizes that
//! knowledge so `backup`, `uninstall`, `doctor`, and an `artifacts` query
//! command can all read from one source.
//!
//! Since the config-root unification (docs/aegis-config-root-unify-design.md),
//! **all** artifacts live under a single root ([`crate::config::config_dir`]).
//! The former split — where `secrets.json`, `peers.json`, `remotes.json`,
//! `checkpoints/`, `wal/` and the intervention files were hardcoded to
//! `~/.aegis` in their own modules — has been removed: every module now routes
//! through `aegis_types::paths::config_dir()`. `aegis doctor` still detects a
//! *legacy* split on machines that predate the unification and advises merging.

use std::path::PathBuf;

/// Coarse category of an artifact — drives `uninstall` keep-choices and
/// `backup` inclusion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactKind {
    /// User configuration (config.toml, SOUL.md, filters, peers, tool defs).
    Config,
    /// Long-term memory (graph, mempalace, WAL).
    Memory,
    /// Strategies + skills (unified).
    Skills,
    /// Long-term goals.
    Goals,
    /// Session history database.
    Sessions,
    /// Credential vault (never backed up unless explicitly requested).
    Secrets,
    /// Run/audit logs.
    Logs,
    /// Transient runtime state (sockets, locks, trash, snapshots, shims).
    Runtime,
    /// Regenerable caches (update checks, imports, overnight records).
    Cache,
}

impl ArtifactKind {
    /// Stable lowercase identifier (used in CLI/JSON output).
    pub fn as_str(&self) -> &'static str {
        match self {
            ArtifactKind::Config => "config",
            ArtifactKind::Memory => "memory",
            ArtifactKind::Skills => "skills",
            ArtifactKind::Goals => "goals",
            ArtifactKind::Sessions => "sessions",
            ArtifactKind::Secrets => "secrets",
            ArtifactKind::Logs => "logs",
            ArtifactKind::Runtime => "runtime",
            ArtifactKind::Cache => "cache",
        }
    }
}

/// Which root directory an artifact lives under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Root {
    /// Resolved via [`crate::config::config_dir`]. Since the config-root
    /// unification (docs/aegis-config-root-unify-design.md), *all* artifacts
    /// live here — the former `~/.aegis` split is gone.
    ConfigDir,
}

impl Root {
    /// Resolve this root to an absolute base path.
    pub fn base(&self) -> PathBuf {
        match self {
            Root::ConfigDir => crate::config::config_dir(),
        }
    }

    /// Stable identifier for output.
    pub fn as_str(&self) -> &'static str {
        match self {
            Root::ConfigDir => "config_dir",
        }
    }
}

/// A single on-disk product of Aegis, with resolved path and metadata.
#[derive(Debug, Clone)]
pub struct Artifact {
    /// Stable identifier, e.g. `"memory"`.
    pub name: &'static str,
    /// Path relative to `root`, e.g. `"memory"` or `"sessions.db"`.
    pub rel: &'static str,
    /// Which root this lives under.
    pub root: Root,
    /// Resolved absolute path.
    pub path: PathBuf,
    /// Category.
    pub kind: ArtifactKind,
    /// Worth including in a backup (vs transient/regenerable).
    pub durable: bool,
    /// Contains credentials — excluded from backups unless explicitly opted in.
    pub sensitive: bool,
    /// Human description.
    pub description: &'static str,
}

impl Artifact {
    /// Whether the artifact currently exists on disk.
    pub fn exists(&self) -> bool {
        self.path.exists()
    }
}

/// Static definition row: `(name, rel, root, kind, durable, sensitive, desc)`.
type Row = (
    &'static str,
    &'static str,
    Root,
    ArtifactKind,
    bool,
    bool,
    &'static str,
);

/// The authoritative table. `path` is resolved at call time in [`all`].
const ROWS: &[Row] = &[
    // ── Root A: config_dir() ──────────────────────────────────────────────
    (
        "config",
        "config.toml",
        Root::ConfigDir,
        ArtifactKind::Config,
        true,
        false,
        "User configuration (may contain api_key)",
    ),
    (
        "soul",
        "SOUL.md",
        Root::ConfigDir,
        ArtifactKind::Config,
        true,
        false,
        "User identity / persona",
    ),
    (
        "filters",
        "filters.toml",
        Root::ConfigDir,
        ArtifactKind::Config,
        true,
        false,
        "Output filter rules",
    ),
    (
        "tools",
        "tools.d",
        Root::ConfigDir,
        ArtifactKind::Config,
        true,
        false,
        "Custom tool scripts",
    ),
    (
        "peers_trust",
        "peers_trust.toml",
        Root::ConfigDir,
        ArtifactKind::Config,
        true,
        false,
        "A2A peer trust levels",
    ),
    (
        "memory",
        "memory",
        Root::ConfigDir,
        ArtifactKind::Memory,
        true,
        false,
        "Main memory graph",
    ),
    (
        "mempalace",
        "mempalace",
        Root::ConfigDir,
        ArtifactKind::Memory,
        true,
        false,
        "Mempalace memory taxonomy",
    ),
    (
        "strategies",
        "strategies",
        Root::ConfigDir,
        ArtifactKind::Skills,
        true,
        false,
        "Strategies + skills",
    ),
    (
        "goals",
        "goals",
        Root::ConfigDir,
        ArtifactKind::Goals,
        true,
        false,
        "Long-term goals",
    ),
    (
        "sessions",
        "sessions.db",
        Root::ConfigDir,
        ArtifactKind::Sessions,
        true,
        false,
        "Session history database",
    ),
    (
        "imports",
        "imports",
        Root::ConfigDir,
        ArtifactKind::Cache,
        false,
        false,
        "Imported external sessions",
    ),
    (
        "overnight",
        "overnight",
        Root::ConfigDir,
        ArtifactKind::Cache,
        false,
        false,
        "Overnight run records",
    ),
    (
        "backups",
        "backups",
        Root::ConfigDir,
        ArtifactKind::Cache,
        false,
        false,
        "Backup output dir (not self-backed-up)",
    ),
    (
        "logs",
        "logs",
        Root::ConfigDir,
        ArtifactKind::Logs,
        false,
        false,
        "Run + audit logs",
    ),
    (
        "trash",
        "trash",
        Root::ConfigDir,
        ArtifactKind::Runtime,
        false,
        false,
        "Recoverable-delete trash",
    ),
    (
        "snapshots",
        "snapshots",
        Root::ConfigDir,
        ArtifactKind::Runtime,
        false,
        false,
        "Working-dir snapshots",
    ),
    (
        "bin",
        "bin",
        Root::ConfigDir,
        ArtifactKind::Runtime,
        false,
        false,
        "PATH shim dir (rm→trash)",
    ),
    (
        "gateway_sock",
        "gateway.sock",
        Root::ConfigDir,
        ArtifactKind::Runtime,
        false,
        false,
        "Gateway control socket (transient)",
    ),
    (
        "gateway_lock",
        "gateway.lock",
        Root::ConfigDir,
        ArtifactKind::Runtime,
        false,
        false,
        "Single-instance lock (transient)",
    ),
    (
        "readline_history",
        "readline_history",
        Root::ConfigDir,
        ArtifactKind::Runtime,
        false,
        false,
        "REPL input history",
    ),
    (
        "swap_state",
        "swap-state.json",
        Root::ConfigDir,
        ArtifactKind::Runtime,
        false,
        false,
        "Self-upgrade swap state",
    ),
    (
        "update_check",
        "update_check.json",
        Root::ConfigDir,
        ArtifactKind::Cache,
        false,
        false,
        "Update-check cache",
    ),
    // ── Formerly hardcoded ~/.aegis, now unified under config_dir() ───────
    (
        "secrets",
        "secrets.json",
        Root::ConfigDir,
        ArtifactKind::Secrets,
        true,
        true,
        "Credential vault (excluded from backup by default)",
    ),
    (
        "peers",
        "peers.json",
        Root::ConfigDir,
        ArtifactKind::Config,
        true,
        false,
        "A2A peers",
    ),
    (
        "remotes",
        "remotes.json",
        Root::ConfigDir,
        ArtifactKind::Config,
        true,
        true,
        "SSH remotes (may contain passwords)",
    ),
    (
        "checkpoints",
        "checkpoints",
        Root::ConfigDir,
        ArtifactKind::Runtime,
        false,
        false,
        "Pre-write file checkpoints",
    ),
    (
        "wal",
        "wal",
        Root::ConfigDir,
        ArtifactKind::Memory,
        false,
        false,
        "Memory write-ahead log",
    ),
    (
        "intervene",
        "intervene.txt",
        Root::ConfigDir,
        ArtifactKind::Runtime,
        false,
        false,
        "Channel intervention file",
    ),
    (
        "keyinfo",
        "keyinfo.txt",
        Root::ConfigDir,
        ArtifactKind::Runtime,
        false,
        false,
        "Channel key-info file",
    ),
];

/// All known artifacts (Root A + Root B), with resolved absolute paths.
pub fn all() -> Vec<Artifact> {
    ROWS.iter()
        .map(
            |&(name, rel, root, kind, durable, sensitive, description)| Artifact {
                name,
                rel,
                root,
                path: root.base().join(rel),
                kind,
                durable,
                sensitive,
                description,
            },
        )
        .collect()
}

/// Artifacts worth backing up. `sensitive` ones are included only when
/// `include_secrets` is true (mirrors the existing `backup` semantics).
pub fn durable(include_secrets: bool) -> Vec<Artifact> {
    all()
        .into_iter()
        .filter(|a| a.durable && (include_secrets || !a.sensitive))
        .collect()
}

/// Artifacts of a given category.
pub fn by_kind(kind: ArtifactKind) -> Vec<Artifact> {
    all().into_iter().filter(|a| a.kind == kind).collect()
}

// ── External artifacts (outside any Aegis root) ───────────────────────────────

/// An artifact Aegis leaves *outside* its config roots — detected by probing,
/// not owned by a fixed path. Reported read-only (never auto-removed).
#[derive(Debug, Clone)]
pub struct ExternalArtifact {
    /// Stable identifier.
    pub name: &'static str,
    /// Resolved path (if applicable).
    pub path: PathBuf,
    /// Whether it currently exists / was detected.
    pub present: bool,
    /// Human description + manual-cleanup hint.
    pub description: String,
}

/// Probe for artifacts Aegis creates outside its config roots: the systemd
/// unit(s), the running binary, and leftover git worktrees. Read-only.
pub fn external_probe() -> Vec<ExternalArtifact> {
    let mut out = Vec::new();

    // systemd user unit
    if let Some(home) = dirs_next::home_dir() {
        let user_unit = home.join(".config/systemd/user/aegis-gateway.service");
        let present = user_unit.exists();
        out.push(ExternalArtifact {
            name: "systemd_user_unit",
            path: user_unit,
            present,
            description: "user gateway service — remove with `aegis gateway uninstall`".to_string(),
        });
    }
    // systemd system unit
    let sys_unit = PathBuf::from("/etc/systemd/system/aegis-gateway.service");
    let sys_present = sys_unit.exists();
    out.push(ExternalArtifact {
        name: "systemd_system_unit",
        path: sys_unit,
        present: sys_present,
        description: "system gateway service — remove with `sudo aegis gateway uninstall --system`"
            .to_string(),
    });

    // running binary
    if let Ok(exe) = std::env::current_exe() {
        out.push(ExternalArtifact {
            name: "binary",
            path: exe.clone(),
            present: exe.exists(),
            description: "the aegis binary itself".to_string(),
        });
    }

    // leftover git worktrees created by spawn_task isolate mode
    let tmp = std::env::temp_dir();
    if let Ok(rd) = std::fs::read_dir(&tmp) {
        for e in rd.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("aegis-wt-") {
                out.push(ExternalArtifact {
                    name: "git_worktree",
                    path: e.path(),
                    present: true,
                    description:
                        "leftover sub-agent git worktree — `git worktree remove` in the source repo"
                            .to_string(),
                });
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_rows_resolve_absolute_paths() {
        for a in all() {
            assert!(
                a.path.is_absolute() || a.path.starts_with("."),
                "{}",
                a.name
            );
            assert!(!a.name.is_empty());
            assert!(!a.rel.is_empty());
        }
    }

    #[test]
    fn names_are_unique() {
        let mut names: Vec<&str> = all().iter().map(|a| a.name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "artifact names must be unique");
    }

    #[test]
    fn durable_excludes_sensitive_by_default() {
        let without = durable(false);
        assert!(without.iter().all(|a| !a.sensitive));
        assert!(without.iter().any(|a| a.name == "memory"));
        assert!(!without.iter().any(|a| a.name == "secrets"));

        let with = durable(true);
        assert!(with.iter().any(|a| a.name == "secrets"));
    }

    #[test]
    fn durable_excludes_transient() {
        let d = durable(true);
        // Runtime/cache items must never be in a backup set.
        assert!(!d.iter().any(|a| a.name == "trash"));
        assert!(!d.iter().any(|a| a.name == "gateway_sock"));
        assert!(!d.iter().any(|a| a.name == "update_check"));
    }

    #[test]
    fn by_kind_groups_correctly() {
        let mem = by_kind(ArtifactKind::Memory);
        assert!(mem.iter().any(|a| a.name == "memory"));
        assert!(mem.iter().any(|a| a.name == "mempalace"));
        assert!(mem.iter().any(|a| a.name == "wal"));

        let skills = by_kind(ArtifactKind::Skills);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "strategies");
    }

    #[test]
    fn all_under_config_dir_after_unification() {
        // Post-unification, every artifact resolves under the single config root.
        assert!(all().iter().all(|a| a.root == Root::ConfigDir));
    }

    #[test]
    fn external_probe_includes_binary_and_units() {
        let ext = external_probe();
        assert!(ext.iter().any(|e| e.name == "binary"));
        assert!(ext.iter().any(|e| e.name == "systemd_system_unit"));
    }
}
