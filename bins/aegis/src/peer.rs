//! `aegis peer` CLI subcommand handlers.
//!
//! Manages A2A peer trust levels stored in `<config_dir>/peers_trust.toml`.
//! See `crates/aegis-core/src/peer_trust.rs` for the storage layer and
//! `devdocs/design-sandbox.md` §"身份感知的权限体系" for the trust model.

use anyhow::Result;

use aegis_core::config::{self, Config};
use aegis_core::peer_trust::{effective_trust, PeerTrustDb};
use aegis_security::TrustLevel;

use crate::cli::PeerAction;

/// Entry point for `aegis peer ...`.
pub fn run(action: PeerAction) -> Result<()> {
    match action {
        PeerAction::List => run_list(),
        PeerAction::Trust {
            agent_id,
            level,
            note,
        } => run_trust(&agent_id, &level, note),
        PeerAction::Revoke { agent_id } => run_revoke(&agent_id),
        PeerAction::Capabilities { agent_id } => run_capabilities(&agent_id),
    }
}

fn run_list() -> Result<()> {
    let db_path = PeerTrustDb::default_path();
    let db = PeerTrustDb::load(&db_path)?;
    let cfg = Config::load(&config::config_path()).unwrap_or_default();

    // Union of agent_ids from both sources.
    let mut all_ids: std::collections::BTreeSet<String> = db.peers.keys().cloned().collect();
    for p in &cfg.peers {
        all_ids.insert(p.name.clone());
    }

    if all_ids.is_empty() {
        println!("No peers registered.");
        println!();
        println!("Add one with: aegis peer trust <agent-id> --level trusted");
        return Ok(());
    }

    println!("{:<30} {:<12} {}", "AGENT_ID", "TRUST", "SOURCE");
    println!("{}", "-".repeat(60));
    for id in &all_ids {
        let trust = effective_trust(&db, &cfg.peers, id);
        let source = if db.peers.contains_key(id) {
            "peers_trust.toml"
        } else {
            "config.toml"
        };
        println!("{id:<30} {:<12} {source}", trust.to_string());
    }
    println!();
    println!("Storage: {}", db_path.display());
    println!("Fallback for unknown peers: read_only");
    Ok(())
}

fn run_trust(agent_id: &str, level: &str, note: Option<String>) -> Result<()> {
    let trust: TrustLevel = level.parse().map_err(anyhow::Error::msg)?;
    let db_path = PeerTrustDb::default_path();
    let mut db = PeerTrustDb::load(&db_path)?;
    let created = db.set(agent_id, trust, note);
    db.save(&db_path)?;

    if created {
        println!("Added peer '{agent_id}' with trust level '{trust}'.");
    } else {
        println!("Updated peer '{agent_id}' to trust level '{trust}'.");
    }
    match trust {
        TrustLevel::Owner => {
            println!();
            println!("⚠️  You granted OWNER trust to an external peer.");
            println!("    This gives them the same privileges as your local CLI.");
            println!("    Revoke with: aegis peer revoke {agent_id}");
        }
        TrustLevel::Trusted => {
            println!();
            println!("⚠️  Trusted peers can invoke terminal-execution tools");
            println!("    (subject to per-call sandbox + approval).");
        }
        _ => {}
    }
    Ok(())
}

fn run_revoke(agent_id: &str) -> Result<()> {
    let db_path = PeerTrustDb::default_path();
    let mut db = PeerTrustDb::load(&db_path)?;
    if db.remove(agent_id) {
        db.save(&db_path)?;
        println!("Revoked peer '{agent_id}' (removed from peers_trust.toml).");

        // Check whether the peer is still declared in config.toml — if so,
        // that trust_level still applies as a fallback.
        let cfg = Config::load(&config::config_path()).unwrap_or_default();
        if let Some(p) = cfg.peers.iter().find(|p| p.name == agent_id) {
            println!();
            println!("Note: peer '{agent_id}' is also declared in config.toml");
            println!(
                "with trust_level = \"{}\"; that value now applies.",
                p.trust_level
            );
            println!("Edit config.toml directly if you also want to remove it there.");
        }
    } else {
        println!("Peer '{agent_id}' was not in peers_trust.toml (nothing to remove).");
    }
    Ok(())
}

fn run_capabilities(agent_id: &str) -> Result<()> {
    let db = PeerTrustDb::load(&PeerTrustDb::default_path()).unwrap_or_default();
    let cfg = Config::load(&config::config_path()).unwrap_or_default();
    let trust = effective_trust(&db, &cfg.peers, agent_id);

    println!("Peer:  {agent_id}");
    println!("Trust: {trust}");
    println!();

    // Describe what this trust level allows in prose. Deliberately in the
    // caller's language (Chinese) since the docs and Rev 2 design speak
    // Chinese; the schema is language-agnostic.
    let (sandbox_baseline, tool_rights) = match trust {
        TrustLevel::Owner => (
            "unrestricted (等同 owner CLI)",
            vec![
                "✓ read_file, write_file, patch, search_files, memory_search",
                "✓ terminal, web_extract, browser, spawn_task",
                "  (YOLO 模式仅在你本人 CLI 下生效，不对外部 peer 生效)",
            ],
        ),
        TrustLevel::Trusted => (
            "compute_workdir (工作目录可写、无网络)",
            vec![
                "✓ read_file, write_file, patch, search_files, memory_search",
                "⚠ terminal (每次调用需你审批)",
                "⚠ web_extract, browser (network_readonly 沙箱 + 审批)",
                "✓ spawn_task (compute_workdir 沙箱)",
            ],
        ),
        TrustLevel::Standard => (
            "compute_workdir + deny ~/.ssh ~/.aws ~/.gnupg 等敏感目录",
            vec![
                "✓ read_file, write_file, patch, search_files, memory_search",
                "⚠ terminal (审批 + 不允许访问敏感 HOME 路径)",
                "⚠ web_extract, browser (审批)",
            ],
        ),
        TrustLevel::Restricted => (
            "parser_offline (只读 + 无网络) + deny 敏感目录",
            vec![
                "✓ read_file, search_files, memory_search, session_search",
                "✗ terminal, browser, remote (hard deny)",
                "⚠ web_extract (审批)",
            ],
        ),
        TrustLevel::ReadOnly => (
            "deny_all (禁止 spawn 任何子进程)",
            vec![
                "✓ read_file, search_files, memory_search, session_search",
                "✗ 所有写工具和 spawn 工具 (hard deny)",
            ],
        ),
    };

    println!("Sandbox baseline: {sandbox_baseline}");
    println!();
    println!("Tool permissions:");
    for line in tool_rights {
        println!("  {line}");
    }
    Ok(())
}
