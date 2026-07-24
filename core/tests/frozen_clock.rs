// The HIVE_TEST_NOW frozen-clock seam (PLAN-v2.1 PR 4.1): `store::now_iso`
// is the command layer's ONE clock, so freezing it freezes every minted
// timestamp — the screenshot tier and app-level tests depend on identical
// pixels/goldens run to run. The determinism fence
// (core/tests/determinism.rs) is untouched by construction: the fenced dirs
// (oplog/blockstore/fold/index) never read env or clocks; only the command
// layer calls now_iso.
//
// ONE test fn on purpose: the env var is process-global and integration test
// files run their fns on parallel threads — a sibling test racing the real
// clock would flake. Everything sequences inside this fn.

mod common;

use serde_json::json;

#[tokio::test]
async fn hive_test_now_freezes_the_command_layer_clock() {
    // Deliberately NOT the 24-char envelope shape: the seam must normalize
    // any RFC 3339 instant through the canonical formatter, or the LogWriter
    // would reject the minted ts at append.
    std::env::set_var("HIVE_TEST_NOW", "2026-01-15T12:00:00Z");
    assert_eq!(hive_core::store::now_iso(), "2026-01-15T12:00:00.000Z");

    // Offsets normalize to UTC (the envelope's only zone) and millis carry.
    std::env::set_var("HIVE_TEST_NOW", "2026-01-15T13:30:00.250+01:30");
    assert_eq!(hive_core::store::now_iso(), "2026-01-15T12:00:00.250Z");

    // A malformed value panics loudly — a silent fallback to the moving
    // clock would turn a harness typo into flaky goldens.
    std::env::set_var("HIVE_TEST_NOW", "yesterday-ish");
    assert!(
        std::panic::catch_unwind(hive_core::store::now_iso).is_err(),
        "malformed HIVE_TEST_NOW must panic, not fall back to the real clock"
    );

    // End to end: a store write stamps the frozen instant (mint → record →
    // fold → read back), proving the seam sits where every ts is minted.
    std::env::set_var("HIVE_TEST_NOW", "2026-01-15T12:00:00Z");
    let store = common::test_store().await;
    let view = store
        .journal_append(
            serde_json::from_value(json!({"body": "written under the frozen clock"})).unwrap(),
            Some("nate"),
            None,
        )
        .await
        .unwrap();
    assert_eq!(view.entry.created_at, "2026-01-15T12:00:00.000Z");

    // Unset: the clock moves again (shape intact).
    std::env::remove_var("HIVE_TEST_NOW");
    let live = hive_core::store::now_iso();
    assert_eq!(live.len(), 24);
    assert_ne!(live, "2026-01-15T12:00:00.000Z");
    store.shutdown().await.unwrap();
}
