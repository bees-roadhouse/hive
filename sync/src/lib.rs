// hive-sync — the replication protocol (PLAN-v2.1 PR 4.4, D29).
//
// The protocol IS the device API: there is no REST surface for humans and no
// second doorway. A session moves two things and nothing else — verbatim
// sealed-segment bytes and ciphertext blocks by bare id — because those are
// the only two artifacts that survive transfer without losing the guarantees
// the frozen formats give them. Segment boundaries feed the frozen key
// derivation and rotation is writer policy, so bytes are copied, never
// re-encoded; blocks are addressed by the blake3 of their ciphertext, so a
// holder with no keys can still verify everything it stores.
//
// Stage A is one-way backup (D29's blind tier), which is why this crate has a
// [`SyncSource`] (a device's store) and a [`SyncSink`] (a directory of
// ciphertext) rather than two symmetric peers: the pushing device never
// ingests foreign records, and the receiving vault folds nothing. Foreign
// ingest is PR 4.10's job; when it lands, the same frames carry it.
//
// Layout:
//
//   [`frame`]    the wire — length-prefixed CBOR control frames + raw payload
//                streams, with every peer-declared length capped before it
//                sizes an allocation
//   [`session`]  the state machine, generic over `AsyncRead + AsyncWrite`
//   [`source`]   what a peer serves; implemented for `hive_core::store::Store`
//   [`sink`]     what a peer lands; implemented by [`DirVault`], a directory
//                in exact store shape (the node's SegmentVault, PR 4.6, is
//                this plus a server's bookkeeping)
//   [`tls`]      the carrier (PR 4.5) — self-signed device certificates
//                pinned by SPKI, under which the sessions above run unchanged
//
// The binary half (`src/main.rs`) is a skeleton: the operator-facing commands
// — `restore` first (PR 4.8) — grow onto this library, and the smoke tier
// drives the library in-proc rather than spawning it
// (docs/TESTING-STRATEGY.md §4.1).

pub mod frame;
pub mod session;
pub mod sink;
pub mod source;
pub mod tls;

pub use frame::{Frame, Hello, ProtoError, Role, PROTO_VERSION};
pub use session::{push_session, receive_session, PushReport, ReceiveReport, SessionConfig};
pub use sink::{DirVault, LandedSegment, SyncSink};
pub use source::SyncSource;
pub use tls::{DeviceCert, PinSet, PinnedKeys, SpkiPin};
