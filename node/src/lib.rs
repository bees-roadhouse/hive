// hive-node — the always-on peer (PLAN-v2.1 PR 4.6, D29/D33/D35).
//
// A node is not a server in the sense the word usually carries. Every device
// is the sole authority for its own log — it mints the gapless seq and the
// hash chain, and no node can rewrite or reorder any of it, only hold it and
// forward it. What a node is, is the ANCHOR: the most complete, always-on
// replica, which is everything "source of truth" means operationally (all the
// data, always reachable, restores flow from it) with none of the
// server-authority semantics (D36).
//
// Stage A ships the BLIND tier and only that: the node holds no key, folds
// nothing, and cannot read a byte of what it stores (D29). That is why this
// crate's core type is a [`vault::SegmentVault`] rather than a Store — a
// directory of verbatim ciphertext in exact store shape, whose one job is to
// never lose or change a byte, plus the bookkeeping and integrity alarms a
// server owes its operator.
//
// Layout:
//
//   [`config`]  `node.toml` + per-tenant `tenant.toml` (name, tier, quotas —
//               and deliberately no IdP/KMS/console fields, D33)
//   [`meta`]    `node-meta.db`, plain SQLite: lengths, hashes, sizes, pinned
//               device keys, control epochs, the pending-forget queue, alarms
//   [`vault`]   the write-once blind vault, `tenants/<t>/domains/<d>/`
//   [`dns`]     the address publisher — `_hive._tcp.<zone>` plus the node's
//               own per-interface-class A/AAAA, and nothing else, ever
//   [`server`]  boot: open the root, resolve the key, discover the vaults,
//               bind, announce, serve
//
// lib + thin `main` on purpose (docs/TESTING-STRATEGY.md §4.1): `node/tests/`
// spawns the real binary for the boot contract, and multi-party smoke
// scenarios embed this library in-process instead of shelling out.
//
// Two fences are enforced from this crate's tests, because this crate is
// where their exception lives (`tests/fences.rs`): sessions/tokens/OIDC may
// exist only below the node binary, and tenancy identifiers may not appear in
// core at all — the data plane is tenancy-blind by construction (D33).

pub mod config;
pub mod dns;
pub mod meta;
pub mod server;
pub mod vault;

pub use config::{
    AddressClass, AdvertisedAddress, DnsConfig, DnsProvider, NodeConfig, Quotas, TenantConfig, Tier,
};
pub use dns::{allowed_names, DnsPublisher, PublishReport, RecordType, ZoneApi};
pub use meta::{Alarm, AlarmKind, DevicePin, NodeMeta, SegmentRow};
pub use server::{Bound, Node};
pub use vault::SegmentVault;
