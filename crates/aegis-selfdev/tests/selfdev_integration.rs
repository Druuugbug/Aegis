//! Offline integration tests for the selfdev protocol.
//!
//! These tests exercise the full selfdev lifecycle without requiring
//! actual git repos, LLM calls, or network access. They validate the
//! state machine, crash detection, canary lifecycle, and build info
//! structures that selfdev.spec.yaml specifies.

use aegis_selfdev::*;
use std::path::PathBuf;

fn test_engine() -> SelfDevEngine {
    SelfDevEngine::new(PathBuf::from("/tmp/aegis-selfdev-test"))
}

// ── Canary state machine contract ────────────────────────────────────

#[test]
fn contract_canary_initial_state_is_none() {
    let engine = test_engine();
    assert!(engine.canary_slot.is_none());
}

#[tokio::test]
async fn contract_canary_deploy_sets_active() {
    let mut engine = test_engine();
    engine
        .deploy_canary(PathBuf::from("/tmp/bin"))
        .await
        .unwrap();
    let slot = engine.canary_slot.as_ref().unwrap();
    assert!(matches!(slot.status, CanaryStatus::Active));
    assert!(engine.canary_start_time.is_some());
}

#[tokio::test]
async fn contract_canary_active_to_promoted() {
    let mut engine = test_engine();
    engine
        .deploy_canary(PathBuf::from("/tmp/bin"))
        .await
        .unwrap();
    engine.promote_canary().await.unwrap();
    let slot = engine.canary_slot.as_ref().unwrap();
    assert!(matches!(slot.status, CanaryStatus::Promoted));
    assert_eq!(engine.stable_binary, PathBuf::from("/tmp/bin"));
    assert!(slot.promoted_at.is_some());
}

#[tokio::test]
async fn contract_canary_active_to_rollback() {
    let mut engine = test_engine();
    engine
        .deploy_canary(PathBuf::from("/tmp/bin"))
        .await
        .unwrap();
    engine.rollback_to_stable("test").await.unwrap();
    let slot = engine.canary_slot.as_ref().unwrap();
    assert!(matches!(slot.status, CanaryStatus::RolledBack));
}

#[tokio::test]
async fn contract_canary_promoted_to_rollback() {
    let mut engine = test_engine();
    engine
        .deploy_canary(PathBuf::from("/tmp/bin"))
        .await
        .unwrap();
    engine.promote_canary().await.unwrap();
    engine.rollback_to_stable("late crash").await.unwrap();
    let slot = engine.canary_slot.as_ref().unwrap();
    assert!(matches!(slot.status, CanaryStatus::RolledBack));
}

#[tokio::test]
async fn contract_promote_without_deploy_errors() {
    let mut engine = test_engine();
    assert!(engine.promote_canary().await.is_err());
}

#[tokio::test]
async fn contract_rollback_without_deploy_noops() {
    let mut engine = test_engine();
    assert!(engine.rollback_to_stable("noop").await.is_ok());
}

// ── Rollback precondition contract ──────────────────────────────────

#[test]
fn contract_no_crashes_no_rollback() {
    let engine = test_engine();
    assert!(!engine.should_rollback());
}

#[test]
fn contract_crashes_without_canary_no_rollback() {
    let mut engine = test_engine();
    engine.record_crash(CrashInfo {
        version: "v1".into(),
        error: "segfault".into(),
        backtrace: None,
        auto_rollback: true,
    });
    assert!(!engine.should_rollback());
}

#[test]
fn contract_crashes_with_building_canary_no_rollback() {
    let mut engine = test_engine();
    engine.record_crash(CrashInfo {
        version: "v1".into(),
        error: "crash".into(),
        backtrace: None,
        auto_rollback: true,
    });
    engine.canary_slot = Some(CanarySlot {
        version: "v1".into(),
        binary_path: PathBuf::from("/tmp/bin"),
        status: CanaryStatus::Building,
        promoted_at: None,
    });
    // Building status shouldn't trigger rollback
    assert!(!engine.should_rollback());
}

#[test]
fn contract_crashes_with_active_canary_triggers_rollback() {
    let mut engine = test_engine();
    engine.record_crash(CrashInfo {
        version: "v1".into(),
        error: "crash".into(),
        backtrace: None,
        auto_rollback: true,
    });
    engine.canary_slot = Some(CanarySlot {
        version: "v1".into(),
        binary_path: PathBuf::from("/tmp/bin"),
        status: CanaryStatus::Active,
        promoted_at: None,
    });
    assert!(engine.should_rollback());
}

#[test]
fn contract_crashes_with_promoted_canary_triggers_rollback() {
    let mut engine = test_engine();
    engine.record_crash(CrashInfo {
        version: "v1".into(),
        error: "crash".into(),
        backtrace: None,
        auto_rollback: true,
    });
    engine.canary_slot = Some(CanarySlot {
        version: "v1".into(),
        binary_path: PathBuf::from("/tmp/bin"),
        status: CanaryStatus::Promoted,
        promoted_at: None,
    });
    assert!(engine.should_rollback());
}

#[test]
fn contract_crashes_with_rolledback_canary_no_rollback() {
    let mut engine = test_engine();
    engine.record_crash(CrashInfo {
        version: "v1".into(),
        error: "crash".into(),
        backtrace: None,
        auto_rollback: true,
    });
    engine.canary_slot = Some(CanarySlot {
        version: "v1".into(),
        binary_path: PathBuf::from("/tmp/bin"),
        status: CanaryStatus::RolledBack,
        promoted_at: None,
    });
    assert!(!engine.should_rollback());
}

// ── Watch canary with zero duration ─────────────────────────────────

#[tokio::test]
async fn contract_watch_no_crash_promotes() {
    let mut engine = test_engine();
    engine
        .deploy_canary(PathBuf::from("/tmp/bin"))
        .await
        .unwrap();
    engine.observe_duration_secs = 0;
    let promoted = engine.watch_canary().await.unwrap();
    assert!(promoted);
    assert!(matches!(
        engine.canary_slot.as_ref().unwrap().status,
        CanaryStatus::Promoted
    ));
}

#[tokio::test]
async fn contract_watch_with_crash_rollback() {
    let mut engine = test_engine();
    engine
        .deploy_canary(PathBuf::from("/tmp/bin"))
        .await
        .unwrap();
    engine.record_crash(CrashInfo {
        version: "v1".into(),
        error: "crash".into(),
        backtrace: None,
        auto_rollback: true,
    });
    engine.observe_duration_secs = 0;
    let promoted = engine.watch_canary().await.unwrap();
    assert!(!promoted);
    assert!(matches!(
        engine.canary_slot.as_ref().unwrap().status,
        CanaryStatus::RolledBack
    ));
}

// ── Build target ────────────────────────────────────────────────────

#[test]
fn contract_build_target_names() {
    assert_eq!(BuildTarget::Aegis.binary_name(), "aegis");
    assert_eq!(BuildTarget::All.binary_name(), "aegis");
}

#[test]
fn contract_build_target_from_str() {
    // Legacy `worker` / `server` values map to the unified aegis binary.
    assert!(matches!(
        BuildTarget::from_str_opt(Some("worker")),
        BuildTarget::Aegis
    ));
    assert!(matches!(
        BuildTarget::from_str_opt(Some("server")),
        BuildTarget::Aegis
    ));
    assert!(matches!(
        BuildTarget::from_str_opt(Some("all")),
        BuildTarget::All
    ));
    assert!(matches!(
        BuildTarget::from_str_opt(None),
        BuildTarget::Aegis
    ));
    assert!(matches!(
        BuildTarget::from_str_opt(Some("xyz")),
        BuildTarget::Aegis
    ));
}

// ── Multi-crash accumulation ────────────────────────────────────────

#[test]
fn contract_multiple_crashes_accumulate() {
    let mut engine = test_engine();
    for i in 0..5 {
        engine.record_crash(CrashInfo {
            version: format!("v{}", i),
            error: format!("error-{}", i),
            backtrace: None,
            auto_rollback: i % 2 == 0,
        });
    }
    assert_eq!(engine.crash_history.len(), 5);
    assert_eq!(engine.crash_history[0].version, "v0");
    assert_eq!(engine.crash_history[4].version, "v4");
}

// ── Full integration lifecycle ───────────────────────────────────────

#[tokio::test]
async fn integration_full_deploy_promote_cycle() {
    let mut engine = test_engine();
    engine
        .deploy_canary(PathBuf::from("/tmp/bin-v1"))
        .await
        .unwrap();

    // Simulate watching without crashes
    engine.observe_duration_secs = 0;
    let promoted = engine.watch_canary().await.unwrap();
    assert!(promoted);
    assert_eq!(engine.stable_binary, PathBuf::from("/tmp/bin-v1"));
}

#[tokio::test]
async fn integration_crash_rollback_redeploy() {
    let mut engine = test_engine();

    // First deploy
    engine
        .deploy_canary(PathBuf::from("/tmp/bin-v1"))
        .await
        .unwrap();
    assert!(matches!(
        engine.canary_slot.as_ref().unwrap().status,
        CanaryStatus::Active
    ));

    // Crash!
    engine.record_crash(CrashInfo {
        version: "v1".into(),
        error: "segfault".into(),
        backtrace: Some("frame1\nframe2".into()),
        auto_rollback: true,
    });
    assert!(engine.should_rollback());

    // Rollback
    engine.rollback_to_stable("segfault").await.unwrap();
    assert!(matches!(
        engine.canary_slot.as_ref().unwrap().status,
        CanaryStatus::RolledBack
    ));

    // Clear crash history for redeploy
    engine.crash_history.clear();

    // Redeploy
    engine
        .deploy_canary(PathBuf::from("/tmp/bin-v2"))
        .await
        .unwrap();
    assert!(matches!(
        engine.canary_slot.as_ref().unwrap().status,
        CanaryStatus::Active
    ));

    // Second attempt succeeds
    engine.observe_duration_secs = 0;
    let promoted = engine.watch_canary().await.unwrap();
    assert!(promoted);
    assert_eq!(engine.stable_binary, PathBuf::from("/tmp/bin-v2"));
}
