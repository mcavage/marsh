//! The production host budgets, unchanged, against slow controlled peers.
//! Each takes over five minutes, so they sit outside `mcp_load_tests` and
//! run only by name (`tests/mcp/run_publication_callers.py --long-*`).
use crate::PublicationKind;
use crate::mcp_load_tests::LoadFixture;
use std::{
    fs,
    time::{Duration, Instant},
};

/// Kit preparation is backend-owned: a cold preparation longer than the
/// host's five-minute command budget still completes the load.
#[test]
#[ignore = "over five minutes; requires prebuilt MARSH_LOAD_TEST_BIN"]
fn cold_preparation_outlasts_the_five_minute_host_budget() {
    let f = LoadFixture::new();
    fs::write(f.path("long-prepare"), b"").unwrap();
    let started = Instant::now();
    let loaded = f.guest("mcp load stable --kit other-kit");
    assert!(
        loaded.status.success(),
        "{}",
        String::from_utf8_lossy(&loaded.stderr)
    );
    assert!(started.elapsed() > Duration::from_mins(5));
    assert_eq!(f.prepared(), 1);
    assert!(f.published());
}

/// Slow stock calls in a failed republish use the separate rollback budget:
/// the prior declaration and registration survive.
#[test]
#[ignore = "over five minutes; requires prebuilt MARSH_LOAD_TEST_BIN"]
fn slow_stock_rollback_keeps_the_prior_publication() {
    let f = LoadFixture::new();
    assert!(f.guest("mcp publish long-rollback -- cat").status.success());
    let declaration = f
        .host
        .declaration_path(&f.session, PublicationKind::Mcp, "long-rollback")
        .unwrap();
    let prior = fs::read(&declaration).unwrap();
    let prior_registration = f.registration();
    fs::write(f.path("stock-delay"), b"35").unwrap();
    let started = Instant::now();
    let failed = f.guest("mcp publish long-rollback --sandbox rollback-slow -- 'printf changed'");
    fs::remove_file(f.path("stock-delay")).unwrap();
    let stderr = String::from_utf8_lossy(&failed.stderr);
    assert!(!failed.status.success());
    assert!(stderr.contains("uncertain"), "{stderr}");
    assert!(stderr.contains("forced load failure"), "{stderr}");
    assert!(started.elapsed() > Duration::from_mins(5));
    assert_eq!(fs::read(&declaration).unwrap(), prior);
    assert_eq!(f.registration(), prior_registration);
    assert!(f.guest("mcp unpublish long-rollback").status.success());
    assert!(!f.path("registration.json").exists());
}
