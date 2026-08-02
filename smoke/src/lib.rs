//! hive-smoke — the cross-component scenario crate (docs/TESTING-STRATEGY.md
//! §4, PLAN-v2.1 PR 4.2).
//!
//! The library is deliberately empty: scenarios live in `tests/`, one file per
//! story, each `#![cfg(unix)]` and each test opening with `require_smoke!()`.
//! What lands HERE rather than in a binary crate's own `tests/` is anything
//! that needs more than one component at once — two device stores, a device
//! plus a node, an app plus a bridge — because `CARGO_BIN_EXE_*` is only
//! visible inside the crate that owns the binary and cannot supply two. Those
//! scenarios take absolute paths from `HIVE_APP_BIN`/`HIVE_BRIDGE_BIN`/
//! `HIVE_NODE_BIN` instead (`hive_smoke_support::require_bin!`), which
//! `scripts/smoke.sh` and the CI `smoke` job export.
//!
//! Single-binary smoke stays in the owning crate: `bridge/tests/stdio.rs`
//! today, `node/tests/boot.rs` when the node lands. That bridge suite keeps
//! driving the LOCAL app socket and nothing else — there is deliberately no
//! bridge→node MCP scenario, because no node MCP endpoint exists or ships in
//! v2.1 (the bridge is a local UDS pump, D25). Wanting one is a scoped
//! DIRECTION amendment with its own tests, not a test-plan side effect.
//!
//! The `seed-fixture` bin for the screenshot tier (PR 5.0) is the first thing
//! that will give this `src/` real contents.
