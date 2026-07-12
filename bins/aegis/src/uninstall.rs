//! `aegis uninstall` — remove Aegis's local state, with an interactive prompt
//! letting the user choose what to keep (memory / skills / sessions / goals).
//!
//! Design: docs/aegis-uninstall-design.md. Key decisions:
//! - skills and strategies are unified under one "skills" choice (the whole
//!   `strategies/` dir is kept or removed together);
//! - deletion is a real, irreversible `remove_dir_all` (no auto-backup) — so
//!   a final confirmation step is mandatory unless `--yes` is passed;
//! - memory, skills, sessions and goals each get their own keep/remove choice;
//! - everything else (config.toml, peers/remotes/secrets, logs, trash,
//!   snapshots, bin shims) is always removed;
//! - on Unix the running binary is best-effort self-removed; on Windows or on
//!   failure we print the path for manual removal.

use anyhow::Result;
use std::path::{Path, PathBuf};

use crate::select;

/// Recursively sum the size of a file or directory (best-effort, for display).
fn path_size(p: &Path) -> u64 {
    let meta = match std::fs::symlink_metadata(p) {
        Ok(m) => m,
        Err(_) => return 0,
    };
    if meta.is_file() {
        return meta.len();
    }
    if meta.is_dir() {
        let mut total = 0;
        if let Ok(rd) = std::fs::read_dir(p) {
            for e in rd.flatten() {
                total += path_size(&e.path());
            }
        }
        return total;
    }
    0
}

fn human_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if bytes >= GB {
        format!("{:.1}G", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1}M", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1}K", bytes as f64 / KB as f64)
    } else {
        format!("{bytes}B")
    }
}

/// `aegis artifacts` — list everything Aegis writes to disk. Read-only.
pub fn run_artifacts(json: bool) -> Result<()> {
    use aegis_core::artifacts;
    let all = artifacts::all();
    let external = artifacts::external_probe();

    if json {
        // Hand-rolled JSON to avoid a serde dependency churn here.
        let mut items = Vec::new();
        for a in &all {
            items.push(format!(
                "{{\"name\":\"{}\",\"path\":{:?},\"root\":\"{}\",\"kind\":\"{}\",\"exists\":{},\"size\":{},\"durable\":{},\"sensitive\":{}}}",
                a.name,
                a.path.display().to_string(),
                a.root.as_str(),
                a.kind.as_str(),
                a.exists(),
                if a.exists() { path_size(&a.path) } else { 0 },
                a.durable,
                a.sensitive
            ));
        }
        let mut ext = Vec::new();
        for e in &external {
            ext.push(format!(
                "{{\"name\":\"{}\",\"path\":{:?},\"present\":{}}}",
                e.name,
                e.path.display().to_string(),
                e.present
            ));
        }
        println!("{{\"artifacts\":[{}],\"external\":[{}]}}", items.join(","), ext.join(","));
        return Ok(());
    }

    println!("Aegis 产物清单（所有 Aegis 写入磁盘的东西）：\n");
    println!(
        "{:<16} {:<9} {:<11} {:<7} {:<8} {}",
        "NAME", "KIND", "ROOT", "EXISTS", "SIZE", "PATH"
    );
    for a in &all {
        let exists = a.exists();
        println!(
            "{:<16} {:<9} {:<11} {:<7} {:<8} {}{}",
            a.name,
            a.kind.as_str(),
            a.root.as_str(),
            if exists { "yes" } else { "-" },
            if exists { human_size(path_size(&a.path)) } else { "-".to_string() },
            a.path.display(),
            if a.sensitive { "  [sensitive]" } else { "" }
        );
    }

    println!("\n外部产物（配置目录之外，需手动处理）：");
    for e in &external {
        println!(
            "  {:<20} {:<8} {}\n      └ {}",
            e.name,
            if e.present { "present" } else { "absent" },
            e.path.display(),
            e.description
        );
    }
    println!(
        "\n提示：`aegis backup` 会备份标记为 durable 的产物（默认排除 sensitive）；\
         `aegis uninstall` 可交互选择保留哪些。"
    );
    Ok(())
}

/// Which components the user chose to keep.
#[derive(Debug, Clone, Copy)]
struct KeepPlan {
    memory: bool,
    skills: bool,
    sessions: bool,
    goals: bool,
}

impl KeepPlan {
    /// Config-dir-relative entries to preserve, given this plan.
    /// Artifact kinds to preserve, given this plan.
    fn kept_kinds(&self) -> Vec<aegis_core::artifacts::ArtifactKind> {
        use aegis_core::artifacts::ArtifactKind;
        let mut kinds = Vec::new();
        if self.memory {
            kinds.push(ArtifactKind::Memory);
        }
        if self.skills {
            kinds.push(ArtifactKind::Skills);
        }
        if self.sessions {
            kinds.push(ArtifactKind::Sessions);
        }
        if self.goals {
            kinds.push(ArtifactKind::Goals);
        }
        kinds
    }

    fn keeps_anything(&self) -> bool {
        self.memory || self.skills || self.sessions || self.goals
    }
}

/// Entry point for `aegis uninstall`.
#[allow(clippy::too_many_arguments)]
pub fn run(
    yes: bool,
    keep_memory: bool,
    keep_skills: bool,
    keep_sessions: bool,
    keep_goals: bool,
    purge: bool,
    dry_run: bool,
) -> Result<()> {
    let cfg_dir = aegis_core::config::config_dir();

    if !cfg_dir.exists() {
        println!("Aegis 配置目录不存在（{}），无需卸载数据。", cfg_dir.display());
        maybe_remove_binary(dry_run);
        return Ok(());
    }

    // Decide the keep plan.
    let plan = if yes {
        // Non-interactive: flags decide. `--purge` forces keep-nothing.
        if purge {
            KeepPlan { memory: false, skills: false, sessions: false, goals: false }
        } else {
            KeepPlan {
                memory: keep_memory,
                skills: keep_skills,
                sessions: keep_sessions,
                goals: keep_goals,
            }
        }
    } else {
        match prompt_plan() {
            Some(p) => p,
            None => {
                println!("已取消，未删除任何内容。");
                return Ok(());
            }
        }
    };

    // Resolve the set of absolute paths to preserve (from the manifest, so
    // keeping "memory" also preserves Root B's wal, etc.).
    let mut kept_paths: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    for kind in plan.kept_kinds() {
        for a in aegis_core::artifacts::by_kind(kind) {
            kept_paths.insert(a.path);
        }
    }

    // What to remove: every entry under config_dir not in the kept set (this
    // also sweeps unknown/stray files). Post-unification all artifacts live
    // under config_dir, so this single pass is complete. (Pre-unification
    // machines with a leftover `~/.aegis` are flagged by `aegis doctor`.)
    let mut to_remove: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(&cfg_dir)?.flatten() {
        let p = entry.path();
        if !kept_paths.contains(&p) {
            to_remove.push(p);
        }
    }

    // Show the plan.
    println!("\n卸载计划（配置目录：{}）：", cfg_dir.display());
    if !plan.keeps_anything() {
        println!("  保留：无（将删除整个配置目录）");
    } else {
        println!("  保留：");
        if plan.memory {
            println!("    - 记忆 (memory / mempalace / wal)");
        }
        if plan.skills {
            println!("    - 技能 (strategies)");
        }
        if plan.sessions {
            println!("    - 会话历史 (sessions.db)");
        }
        if plan.goals {
            println!("    - 目标 (goals)");
        }
    }
    println!("  删除（{} 项，不可恢复）：", to_remove.len());
    for p in &to_remove {
        println!(
            "    - {}",
            p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
        );
    }

    if dry_run {
        println!("\n[dry-run] 未实际删除任何内容。");
        maybe_remove_binary(true);
        return Ok(());
    }

    // Final confirmation (interactive only; `--yes` already implies consent).
    if !yes && !confirm_final() {
        println!("已取消，未删除任何内容。");
        return Ok(());
    }

    // Delete.
    let mut removed = 0usize;
    let mut failed = 0usize;
    for p in &to_remove {
        match remove_path(p) {
            Ok(_) => removed += 1,
            Err(e) => {
                eprintln!("  删除失败 {}: {e}", p.display());
                failed += 1;
            }
        }
    }

    // If nothing was kept, remove the (now-empty) config dir itself too.
    if !plan.keeps_anything() {
        let _ = std::fs::remove_dir_all(&cfg_dir);
    }

    println!("\n已删除 {removed} 项{}。", if failed > 0 { format!("，{failed} 项失败") } else { String::new() });
    if plan.keeps_anything() {
        println!("保留的数据仍在 {}。", cfg_dir.display());
    }

    // Report external artifacts (systemd unit, leftover worktrees) detected on
    // disk — these live outside the config roots and aren't auto-removed.
    let external = aegis_core::artifacts::external_probe();
    let present: Vec<_> = external
        .iter()
        .filter(|e| e.present && e.name != "binary")
        .collect();
    if !present.is_empty() {
        println!("\n检测到以下外部产物（需手动处理）：");
        for e in present {
            println!("  - {} ({})\n      └ {}", e.name, e.path.display(), e.description);
        }
    }

    maybe_remove_binary(false);
    Ok(())
}

/// Ask the four keep/remove questions. Returns `None` if cancelled.
fn prompt_plan() -> Option<KeepPlan> {
    println!("\n即将卸载 Aegis 本地数据。请逐项选择要保留还是删除：");
    let memory = ask_keep("记忆 (memory / mempalace)")?;
    let skills = ask_keep("技能 (strategies，含 skill 与策略)")?;
    let sessions = ask_keep("会话历史 (sessions.db)")?;
    let goals = ask_keep("目标 (goals)")?;
    println!(
        "\n其余状态（config.toml / peers.json / remotes.json / secrets.json / logs / trash / snapshots）\
         将被删除，且不可恢复（不做备份）。"
    );
    Some(KeepPlan { memory, skills, sessions, goals })
}

/// One keep/remove question. `true` = keep, `false` = remove. `None` = cancel.
fn ask_keep(label: &str) -> Option<bool> {
    let items = vec!["保留（默认）".to_string(), "删除".to_string()];
    match select::pick(&format!("是否保留 {label}？"), &items) {
        Some(0) => Some(true),
        Some(1) => Some(false),
        _ => None,
    }
}

/// Final "confirm uninstall" gate. `true` = proceed.
fn confirm_final() -> bool {
    let items = vec!["确认卸载".to_string(), "取消".to_string()];
    matches!(select::pick("确认执行卸载？", &items), Some(0))
}

/// Remove a file or directory tree.
fn remove_path(p: &Path) -> std::io::Result<()> {
    if p.is_dir() {
        std::fs::remove_dir_all(p)
    } else {
        std::fs::remove_file(p)
    }
}

/// Best-effort removal of the running binary (Unix), or a manual-removal hint.
fn maybe_remove_binary(dry_run: bool) {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(_) => return,
    };

    if dry_run {
        println!("[dry-run] 二进制位于 {}（未删除）。", exe.display());
        return;
    }

    // Remind about the resident gateway service *before* removing the binary,
    // since `aegis gateway uninstall` needs the binary to run.
    println!(
        "提示：若之前用 `aegis gateway install` 安装过常驻服务，请先执行 `aegis gateway uninstall` 再删除二进制。"
    );

    #[cfg(unix)]
    {
        // Unix allows unlinking a running executable.
        match std::fs::remove_file(&exe) {
            Ok(_) => println!("已删除二进制 {}。", exe.display()),
            Err(e) => println!(
                "无法自动删除二进制 {}（{e}）。如需彻底卸载，请手动执行：\n  rm {}",
                exe.display(),
                exe.display()
            ),
        }
    }

    #[cfg(not(unix))]
    {
        println!(
            "运行中的可执行文件无法自删除。如需彻底卸载，请手动删除：\n  {}",
            exe.display()
        );
    }
}
