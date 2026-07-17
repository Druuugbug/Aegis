//! Integration tests for the identity → sandbox policy pipeline.
//!
//! These tests validate that `aegis-security::derive_sandbox_policy`
//! interacts correctly with `aegis-sandbox` presets and that the
//! `PeerTrustDb` / `effective_trust` layers compose cleanly.

use aegis_core::config::{peer_trust_level, PeerConfig};
use aegis_core::peer_trust::{effective_trust, PeerTrustDb};
use aegis_security::{derive_sandbox_policy, identity_approval, Approval, Identity, TrustLevel};
use std::path::Path;

#[test]
fn local_owner_gets_unrestricted() {
    let policy = derive_sandbox_policy(&Identity::LocalOwner, "terminal", Path::new("/tmp"));
    assert!(!policy.deny_all);
    // Owner has no fs restrictions from the identity layer.
    assert!(policy.fs.ro.is_empty());
}

#[test]
fn a2a_readonly_denies_terminal() {
    let id = Identity::A2aPeer {
        agent_id: "attacker".into(),
        capabilities: vec![],
        trust: TrustLevel::ReadOnly,
    };
    let policy = derive_sandbox_policy(&id, "terminal", Path::new("/tmp"));
    assert!(policy.deny_all);
}

#[test]
fn group_chat_restricted_denies_shell_tools() {
    let id = Identity::Channel {
        channel: "feishu".into(),
        chat_id: "grp-123".into(),
        is_group: true,
        trust: TrustLevel::Restricted,
    };
    let policy = derive_sandbox_policy(&id, "terminal", Path::new("/tmp"));
    assert!(policy.deny_all);
}

#[test]
fn group_chat_restricted_allows_read_file() {
    let id = Identity::Channel {
        channel: "feishu".into(),
        chat_id: "grp-123".into(),
        is_group: true,
        trust: TrustLevel::Restricted,
    };
    // read_file is not a shell-execution tool → policy is derived,
    // but read_file doesn't actually invoke the sandbox anyway. The
    // point of this test is that `derive_sandbox_policy` doesn't panic
    // for a non-shell tool + Restricted identity.
    let policy = derive_sandbox_policy(&id, "read_file", Path::new("/tmp"));
    // Restricted + non-shell → parser_offline (no rw, no network)
    assert!(!policy.deny_all);
    assert!(policy.fs.rw.is_empty());
}

#[test]
fn non_owner_yolo_ignored() {
    let id = Identity::A2aPeer {
        agent_id: "friend".into(),
        capabilities: vec![],
        trust: TrustLevel::Trusted,
    };
    // Even with global YOLO, a Trusted A2A peer must Ask for terminal.
    assert_eq!(identity_approval(&id, "terminal", true), Approval::Ask);
    // Owner + YOLO → Silent
    assert_eq!(
        identity_approval(&Identity::LocalOwner, "terminal", true),
        Approval::Silent
    );
}

#[test]
fn effective_trust_layers_correctly() {
    let mut db = PeerTrustDb::default();
    db.set("alice", TrustLevel::Trusted, Some("granted 2026-07".into()));

    let config_peers = vec![
        PeerConfig {
            name: "alice".into(),
            trust_level: TrustLevel::Standard, // config has different value
            ..Default::default()
        },
        PeerConfig {
            name: "bob".into(),
            trust_level: TrustLevel::Restricted,
            ..Default::default()
        },
    ];

    // DB overrides config for alice.
    assert_eq!(
        effective_trust(&db, &config_peers, "alice"),
        TrustLevel::Trusted
    );
    // Config used when db is silent.
    assert_eq!(
        effective_trust(&db, &config_peers, "bob"),
        TrustLevel::Restricted
    );
    // Unknown peer → ReadOnly (safest default).
    assert_eq!(
        effective_trust(&db, &config_peers, "eve"),
        TrustLevel::ReadOnly
    );
}

#[test]
fn peer_trust_level_fallback_readonly() {
    // Even with an empty peers list, unknown peers should get ReadOnly.
    let peers: Vec<PeerConfig> = vec![];
    assert_eq!(peer_trust_level(&peers, "anyone"), TrustLevel::ReadOnly);
}

#[test]
fn peer_trust_db_persistence_roundtrip() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("peers_trust.toml");

    let mut db = PeerTrustDb::default();
    db.set(
        "coder-bot",
        TrustLevel::Trusted,
        Some("dev workstation".into()),
    );
    db.set("analyst-bot", TrustLevel::Standard, None);
    db.save(&path).expect("save");

    // Re-load and verify.
    let loaded = PeerTrustDb::load(&path).expect("load");
    assert_eq!(loaded.get("coder-bot"), Some(TrustLevel::Trusted));
    assert_eq!(loaded.get("analyst-bot"), Some(TrustLevel::Standard));

    // Verify metadata survived.
    let entry = loaded.peers.get("coder-bot").expect("entry");
    assert_eq!(entry.note.as_deref(), Some("dev workstation"));
    assert!(entry.updated_at.is_some());
}

#[test]
fn capability_token_to_identity_round_trip() {
    use aegis_a2a::auth::CapabilityToken;

    let token = CapabilityToken::sign(
        "coder-bot",
        vec!["tools/*".into(), "sessions/read".into()],
        "test-secret",
    );
    assert!(token.verify_signature("test-secret"));

    let id = token.to_identity(TrustLevel::Trusted);
    match id {
        Identity::A2aPeer {
            agent_id,
            capabilities,
            trust,
        } => {
            assert_eq!(agent_id, "coder-bot");
            assert_eq!(capabilities.len(), 2);
            assert_eq!(trust, TrustLevel::Trusted);
        }
        _ => panic!("expected A2aPeer identity"),
    }
}
