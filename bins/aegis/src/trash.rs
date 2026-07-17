//! Recoverable deletion: an `rm` PATH shim moves targets into a trash instead
//! of destroying them, so an accidental (or agent-issued) `rm` is undoable via
//! `aegis trash restore`.
//!
//! Layout under `<config_dir>/trash/<id>/`: `payload` (the moved file/dir) +
//! `meta.json` ({ original, trashed_at }). The shim at `<config_dir>/bin/rm`
//! execs `aegis __trash-put`, and `<config_dir>/bin` is prepended to PATH.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Result;
use colored::Colorize;

use crate::cli::TrashAction;

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn trash_dir() -> PathBuf {
    aegis_core::config::config_dir().join("trash")
}

fn bin_dir() -> PathBuf {
    aegis_core::config::config_dir().join("bin")
}

/// Install the `rm` shim and prepend the shim dir to this process's PATH so all
/// child processes (terminal tool, etc.) resolve `rm` to the trash shim.
pub fn install() {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => return,
    };
    let bindir = bin_dir();
    if std::fs::create_dir_all(&bindir).is_err() {
        return;
    }
    let shim = bindir.join("rm");
    let script = format!(
        "#!/bin/sh\n# aegis rm→trash shim — deletions are recoverable via `aegis trash`.\nexec \"{}\" __trash-put \"$@\"\n",
        exe.display()
    );
    if std::fs::write(&shim, script).is_ok() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755));
        }
        let path = std::env::var("PATH").unwrap_or_default();
        if !path.split(':').any(|p| Path::new(p) == bindir) {
            std::env::set_var("PATH", format!("{}:{}", bindir.display(), path));
        }
    }
}

fn absolutize(p: &str) -> PathBuf {
    let pb = PathBuf::from(p);
    if pb.is_absolute() {
        pb
    } else {
        std::env::current_dir().unwrap_or_default().join(pb)
    }
}

/// Refuse to trash catastrophic roots (a second backstop beyond the agent's
/// dangerous-command confirmation).
fn is_protected(abs: &Path) -> bool {
    let s = abs.to_string_lossy();
    let s = s.trim_end_matches('/');
    if s.is_empty() {
        return true; // "/"
    }
    const ROOTS: &[&str] = &[
        "/bin", "/boot", "/dev", "/etc", "/lib", "/lib64", "/proc", "/root", "/run", "/sbin",
        "/sys", "/usr", "/var",
    ];
    if ROOTS.contains(&s) {
        return true;
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() && Path::new(s) == Path::new(home.trim_end_matches('/')) {
            return true;
        }
    }
    false
}

fn next_id() -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{ts}-{}-{n}", std::process::id())
}

/// `aegis __trash-put <rm-args>`: move the targets to trash. Returns a process
/// exit code (0 ok, 1 if something was refused/failed).
pub fn put(args: &[String]) -> i32 {
    // Parse like rm: skip flags, honour `--`.
    let mut paths: Vec<&String> = Vec::new();
    let mut end_flags = false;
    for a in args {
        if !end_flags && a == "--" {
            end_flags = true;
            continue;
        }
        if !end_flags && a.starts_with('-') {
            continue; // -r -f -rf … (move handles files+dirs regardless)
        }
        paths.push(a);
    }

    let mut had_err = false;
    let session = std::env::var("AEGIS_SESSION").unwrap_or_else(|_| "unknown".to_string());
    for p in paths {
        let abs = absolutize(p);
        if is_protected(&abs) {
            eprintln!(
                "aegis trash: refusing to trash protected path: {}",
                abs.display()
            );
            had_err = true;
            continue;
        }
        // -f semantics: ignore missing targets.
        let exists = abs.exists() || abs.symlink_metadata().is_ok();
        if !exists {
            continue;
        }
        let id = next_id();
        let entry = trash_dir().join(&id);
        if std::fs::create_dir_all(&entry).is_err() {
            eprintln!(
                "aegis trash: cannot create trash entry for {}",
                abs.display()
            );
            had_err = true;
            continue;
        }
        let payload = entry.join("payload");
        // `mv` handles directories and cross-filesystem moves robustly.
        let status = std::process::Command::new("mv")
            .arg("--")
            .arg(&abs)
            .arg(&payload)
            .status();
        match status {
            Ok(s) if s.success() => {
                let bytes = dir_size(&payload);
                let meta = serde_json::json!({
                    "original": abs.to_string_lossy(),
                    "trashed_at": chrono::Utc::now().to_rfc3339(),
                    "session": session,
                    "bytes": bytes,
                });
                let _ = std::fs::write(entry.join("meta.json"), meta.to_string());
            }
            _ => {
                eprintln!("aegis trash: failed to move {}", abs.display());
                let _ = std::fs::remove_dir_all(&entry);
                had_err = true;
            }
        }
    }
    prune();
    if had_err {
        1
    } else {
        0
    }
}

/// Recursively sum file sizes (does not follow symlinks).
fn dir_size(p: &Path) -> u64 {
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.file_type().is_dir() => {
            let mut total = 0u64;
            if let Ok(rd) = std::fs::read_dir(p) {
                for e in rd.flatten() {
                    total = total.saturating_add(dir_size(&e.path()));
                }
            }
            total
        }
        Ok(m) if m.file_type().is_file() => m.len(),
        _ => 0,
    }
}

/// Two gates, applied together: keep at most `trash_max_sessions` recent
/// sessions, and keep total size under `trash_max_mb` (always keep the newest).
fn prune() {
    let cfg = aegis_core::config::Config::load(&aegis_core::config::config_path()).ok();
    let max_sessions = cfg
        .as_ref()
        .map(|c| c.security.trash_max_sessions)
        .unwrap_or(20)
        .max(1);
    let max_bytes = cfg
        .as_ref()
        .map(|c| c.security.trash_max_mb)
        .unwrap_or(512)
        .saturating_mul(1024 * 1024);

    let mut entries = read_entries(); // oldest → newest
    entries.reverse(); // newest → oldest

    // Gate 1: most-recent N sessions.
    let mut kept_sessions: Vec<String> = Vec::new();
    let mut survivors: Vec<Entry> = Vec::new();
    for e in entries {
        let known = kept_sessions.iter().any(|s| s == &e.session);
        if !known {
            if kept_sessions.len() >= max_sessions {
                let _ = std::fs::remove_dir_all(trash_dir().join(&e.id));
                continue;
            }
            kept_sessions.push(e.session.clone());
        }
        survivors.push(e);
    }

    // Gate 2: total size cap (newest kept first; always keep the most recent).
    let mut total = 0u64;
    for (i, e) in survivors.iter().enumerate() {
        total = total.saturating_add(e.bytes);
        if i > 0 && total > max_bytes {
            for older in &survivors[i..] {
                let _ = std::fs::remove_dir_all(trash_dir().join(&older.id));
            }
            break;
        }
    }
}

struct Entry {
    id: String,
    original: String,
    trashed_at: String,
    session: String,
    bytes: u64,
}

fn read_entries() -> Vec<Entry> {
    let mut out = Vec::new();
    let dir = trash_dir();
    let rd = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(_) => return out,
    };
    for e in rd.flatten() {
        let path = e.path();
        if !path.is_dir() {
            continue;
        }
        let id = e.file_name().to_string_lossy().to_string();
        let meta = path.join("meta.json");
        let (original, trashed_at, session, bytes) = match std::fs::read_to_string(&meta)
            .ok()
            .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
        {
            Some(v) => (
                v.get("original")
                    .and_then(|x| x.as_str())
                    .unwrap_or("?")
                    .to_string(),
                v.get("trashed_at")
                    .and_then(|x| x.as_str())
                    .unwrap_or("?")
                    .to_string(),
                v.get("session")
                    .and_then(|x| x.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
                v.get("bytes").and_then(|x| x.as_u64()).unwrap_or(0),
            ),
            None => ("?".to_string(), "?".to_string(), "unknown".to_string(), 0),
        };
        out.push(Entry {
            id,
            original,
            trashed_at,
            session,
            bytes,
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

fn restore_one(e: &Entry) -> Result<()> {
    let entry = trash_dir().join(&e.id);
    let payload = entry.join("payload");
    let dest = PathBuf::from(&e.original);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let status = std::process::Command::new("mv")
        .arg("--")
        .arg(&payload)
        .arg(&dest)
        .status()?;
    if !status.success() {
        anyhow::bail!("mv failed restoring {}", e.original);
    }
    let _ = std::fs::remove_dir_all(&entry);
    Ok(())
}

fn human_bytes(b: u64) -> String {
    const U: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b}B")
    } else {
        format!("{v:.1}{}", U[i])
    }
}

pub fn run_trash(action: TrashAction) -> Result<()> {
    match action {
        TrashAction::List { session } => {
            let entries: Vec<Entry> = read_entries()
                .into_iter()
                .filter(|e| session.as_ref().map_or(true, |s| &e.session == s))
                .collect();
            if entries.is_empty() {
                println!("Trash is empty.");
                return Ok(());
            }
            println!("{}", "Trash (most recent last):".bright_cyan());
            for e in &entries {
                println!(
                    "  {}  {}  [{}]  {}  {}",
                    e.id.dimmed(),
                    e.trashed_at.dimmed(),
                    e.session.cyan(),
                    human_bytes(e.bytes).dimmed(),
                    e.original
                );
            }
            println!("  restore: aegis trash restore <id> | all | --session <id>");
        }
        TrashAction::Restore { id, session } => {
            let entries = read_entries();
            let targets: Vec<&Entry> = if let Some(sid) = &session {
                entries.iter().filter(|e| &e.session == sid).collect()
            } else if id == "all" {
                entries.iter().collect()
            } else if !id.is_empty() {
                entries.iter().filter(|e| e.id == id).collect()
            } else {
                println!("Specify an <id>, `all`, or --session <id>.");
                return Ok(());
            };
            if targets.is_empty() {
                println!("No matching trash entries. Use `aegis trash list`.");
                return Ok(());
            }
            let mut n = 0;
            for e in targets {
                match restore_one(e) {
                    Ok(_) => {
                        n += 1;
                        println!("restored {}", e.original);
                    }
                    Err(err) => eprintln!("failed {}: {err}", e.original),
                }
            }
            println!("restored {n} item(s).");
        }
        TrashAction::Empty { days, session } => {
            let entries = read_entries();
            let cutoff = days.map(|d| chrono::Utc::now() - chrono::Duration::days(d as i64));
            let mut n = 0;
            for e in &entries {
                if let Some(sid) = &session {
                    if &e.session != sid {
                        continue;
                    }
                }
                let drop = match cutoff {
                    None => true,
                    Some(c) => chrono::DateTime::parse_from_rfc3339(&e.trashed_at)
                        .map(|t| t.with_timezone(&chrono::Utc) < c)
                        .unwrap_or(true),
                };
                if drop {
                    let _ = std::fs::remove_dir_all(trash_dir().join(&e.id));
                    n += 1;
                }
            }
            println!("emptied {n} item(s) from trash.");
        }
    }
    Ok(())
}
