//! `aegis backup` / `aegis restore` — disaster recovery for aegis's own state.
//!
//! Backs up the curated, durable parts of the config dir (memory, strategies,
//! goals, sessions db, config, peers/remotes) into a single `tar.gz`, excluding
//! transient/large scratch (logs, trash, snapshots, sockets) and — by default —
//! `secrets.json` (so a backup file is never a credential leak). Restore is
//! destructive (overwrites current state), so it auto-saves a pre-restore
//! backup and requires `--force`.

use anyhow::{anyhow, Result};

fn config_dir() -> std::path::PathBuf {
    aegis_core::config::config_dir()
}

/// Durable artifacts that currently exist on disk, as `(root_base, rel)` pairs.
/// Sourced from the central manifest (`aegis_core::artifacts`) so backups never
/// silently miss a newly-added product, and so artifacts that actually live
/// under `~/.aegis` (peers/remotes/secrets) are found at their true root even
/// when `config_dir()` resolves elsewhere. `secrets.json` is included only when
/// `include_secrets` is set.
fn present_entries(include_secrets: bool) -> Vec<(std::path::PathBuf, String)> {
    aegis_core::artifacts::durable(include_secrets)
        .into_iter()
        .filter(|a| a.exists())
        .map(|a| (a.root.base(), a.rel.to_string()))
        .collect()
}

fn timestamp() -> String {
    chrono::Utc::now().format("%Y%m%d-%H%M%S").to_string()
}

/// Append interleaved `-C <base> <rel>` args so tar picks each entry up from
/// its own root directory (entries may span two roots — see manifest §2.2).
fn add_entry_args(cmd: &mut std::process::Command, entries: &[(std::path::PathBuf, String)]) {
    for (base, rel) in entries {
        cmd.arg("-C").arg(base).arg(rel);
    }
}

/// `aegis backup`: write a tar.gz of the durable config state (per manifest).
pub fn run_backup(out: Option<String>, include_secrets: bool) -> Result<()> {
    let base = config_dir();
    let entries = present_entries(include_secrets);
    if entries.is_empty() {
        println!("Nothing to back up (no state found in {}).", base.display());
        return Ok(());
    }
    let out_path = match out {
        Some(p) => std::path::PathBuf::from(p),
        None => {
            let dir = base.join("backups");
            std::fs::create_dir_all(&dir)?;
            dir.join(format!("aegis-backup-{}.tgz", timestamp()))
        }
    };
    if let Some(p) = out_path.parent() {
        std::fs::create_dir_all(p)?;
    }

    let mut cmd = std::process::Command::new("tar");
    cmd.arg("-czf").arg(&out_path);
    add_entry_args(&mut cmd, &entries);
    let status = cmd
        .status()
        .map_err(|e| anyhow!("failed to run tar: {e}"))?;
    if !status.success() {
        return Err(anyhow!("tar failed (exit {:?})", status.code()));
    }
    let size = std::fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0);
    let names: Vec<&str> = entries.iter().map(|(_, rel)| rel.as_str()).collect();
    println!(
        "🧿 backup written: {} ({:.1} MB)\n   included: {}{}",
        out_path.display(),
        size as f64 / (1024.0 * 1024.0),
        names.join(", "),
        if include_secrets {
            "\n   ⚠ includes secrets.json (PLAINTEXT keys) — keep this file private"
        } else {
            "\n   (secrets.json excluded; use --include-secrets to include it)"
        }
    );
    Ok(())
}

/// `aegis restore`: extract a backup over the config dir. Destructive — saves a
/// pre-restore backup first and requires `--force`.
pub fn run_restore(path: String, force: bool) -> Result<()> {
    let archive = std::path::PathBuf::from(&path);
    if !archive.exists() {
        return Err(anyhow!("backup file not found: {}", archive.display()));
    }
    let base = config_dir();

    if !force {
        println!(
            "Restore would OVERWRITE current state in {} with the contents of {}.",
            base.display(),
            archive.display()
        );
        println!("This replaces memory / strategies / goals / sessions / config.");
        println!("Re-run with --force to proceed (a pre-restore backup is saved automatically).");
        return Ok(());
    }

    // Safety net: snapshot the current curated state before overwriting.
    let entries = present_entries(true); // include secrets in the local safety copy
    if !entries.is_empty() {
        let dir = base.join("backups");
        std::fs::create_dir_all(&dir)?;
        let safety = dir.join(format!("pre-restore-{}.tgz", timestamp()));
        let mut cmd = std::process::Command::new("tar");
        cmd.arg("-czf").arg(&safety);
        add_entry_args(&mut cmd, &entries);
        match cmd.status() {
            Ok(s) if s.success() => println!("   saved pre-restore backup: {}", safety.display()),
            _ => println!("   ⚠ could not save a pre-restore backup; continuing"),
        }
    }

    std::fs::create_dir_all(&base)?;
    let status = std::process::Command::new("tar")
        .arg("-xzf")
        .arg(&archive)
        .arg("-C")
        .arg(&base)
        .status()
        .map_err(|e| anyhow!("failed to run tar: {e}"))?;
    if !status.success() {
        return Err(anyhow!("tar extract failed (exit {:?})", status.code()));
    }
    println!("🧿 restored from {} into {}", archive.display(), base.display());
    println!("   restart the gateway to load restored config/memory: aegis gateway stop");
    Ok(())
}
