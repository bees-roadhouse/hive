//! `hive-relay` ... reaching a self-hosted hive from a coffee shop without a
//! domain, a forwarded port, or a reverse proxy.
//!
//! # Shape
//!
//! ```text
//!   browser ──TLS(<id>.relay.example)──▶ Traefik ──passthrough──▶ relay daemon
//!                                                                      │
//!                                                    control + data, dialled
//!                                                    OUTBOUND from the house
//!                                                                      ▼
//!                                                                relay agent
//!                                                            (terminates TLS)
//!                                                                      │
//!                                                                 loopback
//!                                                                      ▼
//!                                                             hive-api :7878
//! ```
//!
//! Traefik matches the SNI in the ClientHello, which is in the clear before the
//! handshake, and forwards the stream without decrypting it. The daemon splices
//! raw sockets. TLS terminates at the agent, inside the house, on a certificate
//! whose private key never leaves it.
//!
//! So the relay operator sees: instance ids, client addresses, timing, and byte
//! counts. Not requests, not responses, not journal prose.
//!
//! # The switch
//!
//! Remote access is opt-in and off by default. There is no relay unless the
//! household runs [`agent`], and the agent refuses to start without an explicit
//! `HIVE_RELAY_ENABLED=1`. Stopping it is the off switch, and it takes effect
//! immediately because the daemon publishes nothing for an instance that is not
//! holding a control connection open.

pub mod agent;
pub mod control;
pub mod daemon;
pub mod limits;
pub mod sni;
pub mod tap;
