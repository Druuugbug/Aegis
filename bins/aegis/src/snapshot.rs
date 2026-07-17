//! Pre-command working-directory snapshots: before a risky command (mv, dd,
//! truncate, git reset --hard, …) the working dir is archived so it can be
//! rolled back. Portable (uses `tar`), bounded by a per-snapshot cwd size cap
//! and a total store cap.
//!
//! Layout: `<config_dir>/snapshots/<id>/payload.tgz` + `meta.json`
//! ({ cwd, command, session, time, bytes }).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Result;
use colored::Colorize;

use crate::cli::SnapshotAction;

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn snap_dir() -> PathBuf {
    aegis_core::config::config_dir().join("snapshots")
}

fn next_id() -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{ts}-{}-{n}", std::process::id())
}

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

/// `aegis __snapshot-cwd <session> <command>`: archive the current working dir
/// as a rollback point (skips if it's larger than the configured cap).
pub fn snapshot_cwd(session: &str, command: &str) -> i32 {
    let cfg = aegis_core::config::Config::load(&aegis_core::config::config_path()).ok();
    let cwd_cap = cfg
        .as_ref()
        .map(|c| c.security.snapshot_cwd_max_mb)
        .unwrap_or(200)
        .saturating_mul(1024 * 1024);

    let cwd = match std::env::current_dir() {
        Ok(c) => c,
        Err(_) => return 0,
    };
    let size = dir_size(&cwd);
    if cwd_cap > 0 && size > cwd_cap {
        eprintln!(
            "aegis snapshot: working dir {} ({}) exceeds cap; skipping rollback point.",
            cwd.display(),
            human_bytes(size)
        );
        return 0;
    }

    let id = next_id();
    let dir = snap_dir().join(&id);
    if std::fs::create_dir_all(&dir).is_err() {
        return 0;
    }
    let payload = dir.join("payload.tgz");
    let status = std::process::Command::new("tar")
        .arg("-czf")
        .arg(&payload)
        .arg("-C")
        .arg(&cwd)
        .arg(".")
        .status();
    match status {
        Ok(s) if s.success() => {
            let bytes = std::fs::metadata(&payload).map(|m| m.len()).unwrap_or(0);
            let meta = serde_json::json!({
                "cwd": cwd.to_string_lossy(),
                "command": command,
                "session": session,
                "time": chrono::Utc::now().to_rfc3339(),
                "bytes": bytes,
            });
            let _ = std::fs::write(dir.join("meta.json"), meta.to_string());
            prune(
                cfg.as_ref()
                    .map(|c| c.security.snapshot_store_mb)
                    .unwrap_or(1024),
            );
        }
        _ => {
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
    0
}

fn prune(store_mb: u64) {
    let cap = store_mb.saturating_mul(1024 * 1024);
    if cap == 0 {
        return;
    }
    let mut entries = read_entries(); // oldest → newest
    entries.reverse(); // newest → oldest
    let mut total = 0u64;
    for (i, e) in entries.iter().enumerate() {
        total = total.saturating_add(e.bytes);
        if i > 0 && total > cap {
            for older in &entries[i..] {
                let _ = std::fs::remove_dir_all(snap_dir().join(&older.id));
            }
            break;
        }
    }
}

struct Entry {
    id: String,
    cwd: String,
    command: String,
    session: String,
    time: String,
    bytes: u64,
}

fn read_entries() -> Vec<Entry> {
    let mut out = Vec::new();
    let rd = match std::fs::read_dir(snap_dir()) {
        Ok(rd) => rd,
        Err(_) => return out,
    };
    for e in rd.flatten() {
        let path = e.path();
        if !path.is_dir() {
            continue;
        }
        let id = e.file_name().to_string_lossy().to_string();
        let v = std::fs::read_to_string(path.join("meta.json"))
            .ok()
            .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok());
        let get = |k: &str| {
            v.as_ref()
                .and_then(|v| v.get(k))
                .and_then(|x| x.as_str())
                .unwrap_or("?")
                .to_string()
        };
        let bytes = v
            .as_ref()
            .and_then(|v| v.get("bytes"))
            .and_then(|x| x.as_u64())
            .unwrap_or(0);
        out.push(Entry {
            id,
            cwd: get("cwd"),
            command: get("command"),
            session: get("session"),
            time: get("time"),
            bytes,
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

pub fn run_snapshot(action: SnapshotAction) -> Result<()> {
    match action {
        SnapshotAction::List { session } => {
            let entries: Vec<Entry> = read_entries()
                .into_iter()
                .filter(|e| session.as_ref().map_or(true, |s| &e.session == s))
                .collect();
            if entries.is_empty() {
                println!("No snapshots.");
                return Ok(());
            }
            println!("{}", "Snapshots (most recent last):".bright_cyan());
            for e in &entries {
                println!(
                    "  {}  {}  [{}]  {}  {}\n      {} {}",
                    e.id.dimmed(),
                    e.time.dimmed(),
                    e.session.cyan(),
                    human_bytes(e.bytes).dimmed(),
                    e.cwd,
                    "cmd:".dimmed(),
                    e.command.dimmed()
                );
            }
            println!("  restore: aegis snapshot restore <id>   (extracts over its working dir)");
        }
        SnapshotAction::Restore { id } => {
            let entries = read_entries();
            match entries.iter().find(|e| e.id == id) {
                Some(e) => {
                    let payload = snap_dir().join(&e.id).join("payload.tgz");
                    let dest = PathBuf::from(&e.cwd);
                    std::fs::create_dir_all(&dest)?;
                    let status = std::process::Command::new("tar")
                        .arg("-xzf")
                        .arg(&payload)
                        .arg("-C")
                        .arg(&dest)
                        .status()?;
                    if status.success() {
                        println!(
                            "restored snapshot {} → {} (extracted over existing files)",
                            id, e.cwd
                        );
                    } else {
                        anyhow::bail!("tar extract failed");
                    }
                }
                None => println!("no snapshot with id {id}. Use `aegis snapshot list`."),
            }
        }
        SnapshotAction::Empty { session } => {
            let entries = read_entries();
            let mut n = 0;
            for e in &entries {
                if session.as_ref().map_or(true, |s| &e.session == s) {
                    let _ = std::fs::remove_dir_all(snap_dir().join(&e.id));
                    n += 1;
                }
            }
            println!("removed {n} snapshot(s).");
        }
    }
    Ok(())
}
