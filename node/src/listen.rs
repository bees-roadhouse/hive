// The listener (PLAN-v2.1 PR 4.7, D29/D30): the node's one door, and
// everything that has to be true before a byte of a domain's ciphertext moves
// through it.
//
// The carrier is PR 4.5's: TLS 1.3, mutual auth, self-signed certificates that
// CARRY a device key rather than chaining to anything, and a pin — blake3 of
// the SPKI — as the whole verdict. What this file adds is who the pins come
// from and what happens after the handshake.
//
// ── The guest list is node-meta, re-read per connection ─────────────────────
//
// A `PinnedKeys` implementation could have been a set decided at boot. It is
// not, and that is the point: `hive-node device revoke` must take effect on
// the NEXT handshake, on a node nobody is going to restart. So each connection
// snapshots the pins (a household's worth of rows) before its handshake and
// builds a [`NodeGate`] from them.
//
// The gate has one door and one hinge:
//
//   * PINNED, UNREVOKED devices are always let through the handshake. Which
//     domain they may then address is not their choice — it follows from the
//     pin, because a pin lives in exactly one domain's node-meta (D29's
//     hub-only boundary, enforced before the session's own domain check).
//   * While a LIVE enrollment code exists, the door is open to keys nobody has
//     seen yet. It has to be: enrollment is how a key becomes known, and there
//     is no pin to check it against beforehand. The ten-minute window an
//     operator opened deliberately is the authorization; the code, the channel
//     binding, and the policy in `enroll.rs` are what a stranger then has to
//     get past. With no code outstanding, an unpinned key cannot complete a
//     handshake at all.
//
//     That door is NODE-WIDE, not per-domain, and it cannot be otherwise: the
//     handshake happens before any frame, so there is no code yet and nothing
//     to scope the exception to. What a stranger gets from it is one opening
//     frame under `MAX_OPENING_FRAME`, inside the handshake deadline, against
//     the connection cap — it reaches no domain's data, because a `hello` needs
//     a pin and an `enroll` routes by a code it does not have. What bounds it
//     instead is WHO MAY OPEN IT: `enroll::mint` refuses a domain that already
//     has an enrolled device, so the window only ever opens for a domain still
//     waiting for its first one, and a fully-enrolled node cannot be put behind
//     an open door at all.
//
// A rustls `ServerConfig` is therefore built per connection rather than once
// per listener. That is a few hundred microseconds against a TLS handshake and
// a SQLite read, and it is what buys "revoke takes effect now" — a listener
// whose guest list was fixed at boot would need a restart to forget a device,
// which is the one moment an operator cannot afford to wait for one.
//
// ── After the handshake, one frame decides ─────────────────────────────────
//
// The first frame is read under [`MAX_OPENING_FRAME`] — a tighter cap than the
// session's, because nothing has been authorized yet — and it is one of two
// things: an `enroll` (a device redeeming a code) or a `hello` (a device that
// is already pinned, opening a push). Anything else is a refusal, audited.
//
// ── Everything is audited, and to a domain where possible ──────────────────
//
// Every refusal lands in `node-meta.db`'s `auth_audit` for the domain it
// concerns, because a refusal an operator cannot read afterwards is not
// auth-failure logging. Two cases have no domain to land in by routing, and
// both are still recorded: a session refused for an unknown pin (journal — no
// domain claims that key), and an enrollment whose code no domain minted,
// which goes to the trail of every domain with an OPEN enrollment window,
// since those are the ones this connection got in on and the ones whose
// operator is currently expecting a device.
//
// ── What bounds a stranger ──────────────────────────────────────────────────
//
// Three limits, all of them about the node staying up rather than about
// authentication:
//
//   * a handshake DEADLINE, so a peer that connects and says nothing costs a
//     slot for seconds rather than forever;
//   * an accept RATE (token bucket), so a connect flood costs one TCP accept
//     each instead of a TLS handshake each;
//   * a connection CAP, so the honest failure mode of the two above is a
//     refused connection and never an exhausted file table.
//
// They are constants, not `node.toml` fields: a knob with no operator asking
// for it is a schema to maintain forever (D33's discipline, applied to the
// listener).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use hive_core::oplog::device_id_ok;
use hive_sync::frame::{
    err_code, read_frame_capped, write_frame, Frame, Hello, ProtoError, MAX_OPENING_FRAME,
};
use hive_sync::tls::{self, DeviceCert, PinSet, PinnedKeys, SpkiPin};
use hive_sync::{receive_session_with_hello, SessionConfig};
use tokio::net::TcpStream;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::enroll;
use crate::meta::{unix_seconds, AuthEventKind, DevicePin};
use crate::server::{Bound, Node};
use crate::vault::SegmentVault;

/// How long a peer has to complete the TLS handshake AND send its opening
/// frame. Generous for a slow link, finite for a peer that connects and waits.
pub const HANDSHAKE_DEADLINE: Duration = Duration::from_secs(10);

/// Connections served at once. A household node talks to a handful of devices;
/// the cap exists so that the failure mode of a flood is "refused", not "out
/// of file descriptors".
pub const MAX_CONNECTIONS: usize = 64;

/// Accepts allowed in a burst before the rate limiter bites.
pub const ACCEPT_BURST: u32 = 32;

/// Steady-state accepts per second. A device reconnecting after a network
/// change costs a handful; anything sustained above this is not a device.
pub const ACCEPT_PER_SECOND: f64 = 8.0;

/// What one `serve` did. Per-connection outcomes are not here on purpose —
/// they go to the domain's audit trail (`node-meta.db`), which is what an
/// operator reads and what the ADVERSARIAL-SMOKE scenarios assert against.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ServeReport {
    /// Connections accepted and handed to a task.
    pub accepted: u64,
    /// Dropped by the accept-rate limiter, before any TLS work.
    pub rate_limited: u64,
    /// Dropped because [`MAX_CONNECTIONS`] were already in flight.
    pub over_capacity: u64,
}

/// A token bucket over accepts.
///
/// Pure in its clock (`allow` takes the instant), so the policy is unit-tested
/// without sleeping — the same shape the enrollment TTL takes.
#[derive(Debug)]
pub struct AcceptRate {
    tokens: f64,
    burst: f64,
    per_second: f64,
    last: Instant,
}

impl AcceptRate {
    pub fn new(burst: u32, per_second: f64, now: Instant) -> AcceptRate {
        AcceptRate {
            tokens: burst as f64,
            burst: burst as f64,
            per_second,
            last: now,
        }
    }

    /// Whether one more accept is allowed at `now`, spending a token if so.
    pub fn allow(&mut self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + elapsed * self.per_second).min(self.burst);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// The node's answer to "may this key complete a handshake?", as of one
/// connection. See the module header for why it is a snapshot and why an open
/// enrollment window widens it.
#[derive(Debug)]
pub struct NodeGate {
    pins: PinSet,
    enrolling: bool,
}

impl NodeGate {
    pub fn new(pins: PinSet, enrolling: bool) -> NodeGate {
        NodeGate { pins, enrolling }
    }

    /// Whether this gate would let a key through only because an enrollment
    /// window is open — the thing an audit line wants to say.
    pub fn is_open_door(&self) -> bool {
        self.enrolling
    }
}

impl PinnedKeys for NodeGate {
    fn accepts(&self, pin: &SpkiPin) -> bool {
        self.pins.accepts(pin) || self.enrolling
    }
}

impl Node {
    /// This node's transport certificate — deterministic in its identity, so
    /// minting it once per listener and once per reboot are the same thing.
    pub fn transport_cert(&self) -> Result<DeviceCert> {
        DeviceCert::self_signed(self.identity()).context("minting the node's transport certificate")
    }

    /// The gate for one connection: every unrevoked pin this node holds, plus
    /// whether any domain has a live enrollment code.
    pub fn gate(&self, now: SystemTime) -> Result<NodeGate> {
        // A clock this node cannot read is a refusal, not a zero: see
        // `meta::unix_seconds`.
        let now = unix_seconds(now)?;
        let mut pins = Vec::new();
        let mut enrolling = false;
        for vault in self.vaults() {
            for pin in vault.meta().device_pins()? {
                if !pin.revoked {
                    pins.push(SpkiPin::from_ed25519_public(&pin.ed25519_pk));
                }
            }
            enrolling |= vault.meta().live_codes(now)? > 0;
        }
        Ok(NodeGate::new(PinSet::new(pins), enrolling))
    }

    /// The domain a pinned device belongs to. A pin lives in exactly one
    /// domain's node-meta, which is what makes "a session authenticated for
    /// one domain cannot reach another" a property of the pin rather than of
    /// anything the peer says.
    ///
    /// A revoked pin resolves to nothing: the session path treats it as
    /// unknown, which is the same refusal an unpinned key gets.
    pub fn vault_for_pin(&self, pin: &SpkiPin) -> Result<Option<(SegmentVault, DevicePin)>> {
        Ok(self
            .vault_for_known_pin(pin)?
            .filter(|(_, row)| !row.revoked))
    }

    /// The domain a pin is ON FILE in, revoked or not.
    ///
    /// [`vault_for_pin`](Self::vault_for_pin) deliberately resolves a revoked
    /// pin to nothing, which is the right answer for authorization and the
    /// wrong one for the audit trail: a revoked device trying to come back is
    /// the single most interesting thing an operator can read after a theft,
    /// and routing it to "no domain" meant it was logged nowhere at all. The
    /// tombstone still says which domain it belongs to, so the refusal can be
    /// written where the person who revoked it will look.
    pub fn vault_for_known_pin(&self, pin: &SpkiPin) -> Result<Option<(SegmentVault, DevicePin)>> {
        for vault in self.vaults() {
            for row in vault.meta().device_pins()? {
                if SpkiPin::from_ed25519_public(&row.ed25519_pk) == *pin {
                    return Ok(Some((vault.clone(), row)));
                }
            }
        }
        Ok(None)
    }

    /// The domain a code was minted for. Routing only — whether the code is
    /// still SPENDABLE is `enroll::redeem`'s atomic business, and an expired
    /// or spent code deliberately still routes home so its refusal lands in
    /// the audit trail of the domain it was minted for.
    pub fn vault_for_code(&self, code: &str) -> Result<Option<SegmentVault>> {
        let hash = hive_sync::enroll::code_hash(&hive_sync::enroll::normalize_code(code)?);
        for vault in self.vaults() {
            if vault.meta().knows_code(&hash)? {
                return Ok(Some(vault.clone()));
            }
        }
        Ok(None)
    }
}

impl Bound {
    /// Accept mTLS connections until `shutdown` resolves.
    ///
    /// Each connection runs in its own task holding a capacity permit; the
    /// tasks are owned by a `JoinSet`, so returning from here drops them and a
    /// transfer that was mid-segment is simply resumed next session (the vault
    /// is append-only and every session re-asks from its landed offset).
    pub async fn serve(
        self,
        node: Arc<Node>,
        shutdown: impl std::future::Future<Output = ()>,
    ) -> Result<ServeReport> {
        let cert = Arc::new(node.transport_cert()?);
        let capacity = Arc::new(Semaphore::new(MAX_CONNECTIONS));
        let mut rate = AcceptRate::new(ACCEPT_BURST, ACCEPT_PER_SECOND, Instant::now());
        let mut limiting = false;
        let mut report = ServeReport::default();
        let mut connections = JoinSet::new();
        let tcp = self.into_listener();

        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                _ = &mut shutdown => return Ok(report),
                // Reaped so a long-lived listener does not accumulate handles.
                // Nothing to collect: a connection's outcome is its audit row.
                Some(_) = connections.join_next(), if !connections.is_empty() => {}
                accepted = tcp.accept() => match accepted {
                    Ok((stream, peer)) => {
                        if !rate.allow(Instant::now()) {
                            report.rate_limited += 1;
                            if !limiting {
                                limiting = true;
                                tracing::warn!(%peer, "accept rate limit reached — refusing \
                                                       connections without handshaking them");
                            }
                            drop(stream);
                            continue;
                        }
                        limiting = false;
                        let Ok(permit) = capacity.clone().try_acquire_owned() else {
                            report.over_capacity += 1;
                            tracing::warn!(%peer, "at {MAX_CONNECTIONS} connections — refusing");
                            drop(stream);
                            continue;
                        };
                        report.accepted += 1;
                        let node = node.clone();
                        let cert = cert.clone();
                        connections.spawn(async move {
                            let _permit = permit;
                            if let Err(e) = serve_connection(node, cert, stream, peer).await {
                                // Every refusal that reached a domain is already
                                // in its audit trail; this is the rest — a failed
                                // handshake, a dropped socket, a peer that said
                                // nothing. WARN and not DEBUG: an operator
                                // chasing "why will my laptop not connect" needs
                                // to see it without turning a filter up.
                                tracing::warn!(%peer, "connection refused or lost: {e:#}");
                            }
                        });
                    }
                    // A per-connection accept error (fd exhaustion, a peer that
                    // vanished between the SYN and the accept) must not take the
                    // listener down: the node is the always-on half of the pair.
                    Err(e) => tracing::warn!("accept failed: {e}"),
                },
            }
        }
    }
}

/// One connection, from TLS handshake to whichever half it turned out to be.
async fn serve_connection(
    node: Arc<Node>,
    cert: Arc<DeviceCert>,
    stream: TcpStream,
    peer: SocketAddr,
) -> Result<()> {
    // node-meta reads are SQLite; they run on a blocking thread, and this one
    // runs BEFORE the handshake so that nothing inside rustls' verifier
    // touches a database while the reactor waits on it.
    let gate = meta_op(&node, |node| node.gate(SystemTime::now())).await?;
    let open_door = gate.is_open_door();
    let acceptor = tls::acceptor(&cert, Arc::new(gate))?;

    let (stream, pin) =
        tokio::time::timeout(HANDSHAKE_DEADLINE, tls::accept_pinned(&acceptor, stream))
            .await
            .context("the peer did not complete a TLS handshake in time")??;
    tracing::debug!(%peer, %pin, enrolling = open_door, "handshake complete");
    let (mut reader, mut writer) = tokio::io::split(stream);

    let opening = tokio::time::timeout(
        HANDSHAKE_DEADLINE,
        read_frame_capped(&mut reader, MAX_OPENING_FRAME),
    )
    .await
    .context("the peer completed a handshake and then said nothing")??;

    match opening {
        Frame::Enroll(request) => {
            serve_enrollment(&node, request, pin, open_door, &mut writer).await
        }
        Frame::Hello(hello) => serve_session(&node, hello, pin, reader, writer).await,
        other => {
            let why = format!(
                "a {} frame cannot open a connection — a session opens with a hello, an \
                 enrollment with an enroll",
                other.variant()
            );
            audit_refusal(&node, pin, None, &why).await?;
            refuse(&mut writer, err_code::UNEXPECTED, why).await
        }
    }
}

/// Run one node-meta operation on a blocking thread. Every SQLite touch on the
/// connection path goes through here — the reads are small, but a socket
/// thread is not where a file lock belongs (the same discipline
/// `SegmentVault`'s `SyncSink` impl follows).
async fn meta_op<T, F>(node: &Arc<Node>, op: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&Node) -> Result<T> + Send + 'static,
{
    let node = node.clone();
    tokio::task::spawn_blocking(move || op(&node))
        .await
        .context("node-meta task")?
}

/// Write a refusal to the audit trail of the domain it concerns — or, when the
/// pin belongs to no domain, to the journal, which is then the only honest
/// place for it.
async fn audit_refusal(
    node: &Arc<Node>,
    pin: SpkiPin,
    claimed: Option<&str>,
    why: &str,
) -> Result<()> {
    let claimed = claimed.unwrap_or("-").to_string();
    let detail = why.to_string();
    let recorded = meta_op(node, move |node| {
        let Some((vault, row)) = node.vault_for_pin(&pin)? else {
            return Ok(false);
        };
        vault.meta().record_auth_event(
            AuthEventKind::SessionRefused,
            Some(&row.device),
            Some(&pin.to_string()),
            &detail,
        )?;
        Ok(true)
    })
    .await?;
    if !recorded {
        tracing::warn!(%pin, %claimed, "{why}");
    }
    Ok(())
}

/// The enrollment half: route the code home, let policy decide, answer.
async fn serve_enrollment<W>(
    node: &Arc<Node>,
    request: hive_sync::EnrollRequest,
    pin: SpkiPin,
    open_door: bool,
    writer: &mut W,
) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    // Belt to the gate's braces: a connection that got in on a pin rather than
    // on an open enrollment window has no business redeeming anything, and the
    // two facts are decided in different places.
    if !open_door {
        let why = "an enrollment arrived with no enrollment window open".to_string();
        tracing::warn!(%pin, device = %request.device, "{why}");
        return refuse(writer, err_code::ENROLLMENT, why).await;
    }

    // Route, decide, and audit in ONE blocking hop: the routing lookup and the
    // policy that follows it read and write the same database, and splitting
    // them across await points would only widen the window a second redemption
    // could race through (the spend itself is atomic either way).
    //
    // `Ok(None)` means refused-and-already-audited. What a refused peer is
    // told is [`ENROLL_REFUSAL`] in every case, so nothing here needs to
    // survive the hop except whether there is a grant.
    let node_pk = node.identity().ed25519_public();
    let grant = meta_op(node, move |node| {
        let now = SystemTime::now();
        // A code that does not even normalize is an auth failure like any
        // other, not a reason to abandon the connection. Propagating that error
        // dropped the peer with NO audit row and NO wire answer, so a stranger
        // could probe an open window with empty or oversized codes and leave
        // the operator's trail completely silent, while every well-formed bad
        // code was dutifully recorded.
        let routed = node.vault_for_code(&request.code).unwrap_or_default();
        match routed {
            Some(vault) => match enroll::redeem(&vault, node_pk, &request, pin, now) {
                // `redeem` has already written the specific reason to the
                // vault's audit trail.
                Err(_) => Ok(None),
                Ok(grant) => Ok(Some(grant)),
            },
            // No domain minted this code, so no domain owns the event by
            // routing. It is still an auth failure someone has to be able to
            // read, so it lands in the trail of every domain whose enrollment
            // window is open — those are the ones this connection got in on,
            // and the ones whose operator is currently expecting a device.
            None => {
                audit_unrouted_code(node, &request.device, pin, now)?;
                Ok(None)
            }
        }
    })
    .await?;

    match grant {
        Some(grant) => {
            write_frame(writer, &Frame::Enrolled(grant))
                .await
                .context("answering an enrollment")?;
            Ok(())
        }
        None => refuse(writer, err_code::ENROLLMENT, ENROLL_REFUSAL.to_string()).await,
    }
}

/// Record "a code no domain minted was presented" wherever it can be read.
///
/// Blocking; called only from inside a [`meta_op`] hop.
fn audit_unrouted_code(node: &Node, device: &str, pin: SpkiPin, now: SystemTime) -> Result<()> {
    // This is the ONE audit path that runs before any policy has looked at the
    // request, so the device id here is still whatever the peer sent — up to
    // MAX_OPENING_FRAME of it. Writing that verbatim let a stranger who holds
    // no valid code push multi-kilobyte rows at the accept rate until
    // AUTH_AUDIT_KEEP evicted the real evidence, from every domain with an open
    // window, including domains it was never routed to. Naming it only when it
    // is a name keeps the row bounded and the trail readable.
    let device = if device_id_ok(device) {
        device
    } else {
        "<malformed device id>"
    };
    let why = format!(
        "an enrollment for device {device:?} presented a code no domain on this node minted"
    );
    let now = unix_seconds(now)?;
    let mut recorded = false;
    for vault in node.vaults() {
        if vault.meta().live_codes(now)? == 0 {
            continue;
        }
        vault.meta().record_auth_event(
            AuthEventKind::EnrollRefused,
            Some(device),
            Some(&pin.to_string()),
            &why,
        )?;
        recorded = true;
    }
    if !recorded {
        tracing::warn!(%pin, %device, "{why}");
    }
    Ok(())
}

/// The one sentence every refused redemption gets. Wrong, expired, reused,
/// relayed, or against policy — all the same on the wire, because a redeemer
/// that could tell them apart could probe the code space. The distinction
/// lives in the operator's audit trail.
const ENROLL_REFUSAL: &str = "enrollment refused";

/// The session half: the pin says which domain, then PR 4.4's receive session
/// runs unchanged.
async fn serve_session<R, W>(
    node: &Arc<Node>,
    hello: Hello,
    pin: SpkiPin,
    reader: R,
    mut writer: W,
) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let Some((vault, row)) = meta_op(node, move |node| node.vault_for_pin(&pin)).await? else {
        // Only reachable while an enrollment window is open — otherwise the
        // handshake itself refused this key. Either way it is the same answer.
        let why = format!(
            "a session opened from an unpinned or revoked device key (it claimed {:?})",
            hello.peer
        );
        // A REVOKED key still names the domain that revoked it, and that is the
        // trail its operator reads after a theft. Logging this to tracing only
        // meant a stolen device probing its way back left no record where
        // anyone would look for one.
        let recorded = {
            meta_op(node, move |node| {
                let Some((vault, row)) = node.vault_for_known_pin(&pin)? else {
                    return Ok(false);
                };
                vault.meta().record_auth_event(
                    AuthEventKind::SessionRefused,
                    Some(&row.device),
                    Some(&pin.to_string()),
                    &format!(
                        "device {:?} was revoked from {}/{} and tried to open a session anyway",
                        row.device,
                        vault.tenant(),
                        vault.domain()
                    ),
                )?;
                Ok(true)
            })
            .await
        };
        match recorded {
            Ok(true) => {}
            // Genuinely unknown to every domain: there is no trail that owns
            // it, so the journal is the honest place.
            Ok(false) => tracing::warn!(%pin, "{why}"),
            Err(e) => tracing::error!(%pin, "could not record a refused session ({why}): {e:#}"),
        }
        return refuse(&mut writer, err_code::REFUSED, why).await;
    };

    let opened = {
        let vault = vault.clone();
        let device = row.device.clone();
        meta_op(node, move |_| {
            vault.meta().record_auth_event(
                AuthEventKind::SessionOpened,
                Some(&device),
                Some(&pin.to_string()),
                &format!(
                    "session opened for {}/{} by device {device:?}",
                    vault.tenant(),
                    vault.domain()
                ),
            )
        })
        .await
    };
    opened.context("recording an opened session")?;

    // The domain is the PIN's, never the hello's. A hello naming a different
    // one is refused by the session's own check — which is exactly the
    // wrong-domain scoping property (docs/TESTING-STRATEGY.md §4.3), and it is
    // a property of the pin rather than of anything the peer said.
    let cfg = SessionConfig::new(vault.domain(), node.node_key_hex());
    let outcome = receive_session_with_hello(reader, writer, &vault, &cfg, hello).await;
    // `session_opened` is written before the hello is validated, because the
    // pin is what the row is about. That left the cross-domain probe — the one
    // D29 boundary an operator most wants to see — reading as a healthy session
    // with no refusal after it, so a repeated campaign looked like normal
    // traffic. Whatever the session decided is recorded next to the open.
    let report = match outcome {
        Ok(report) => report,
        Err(e) => {
            let why = format!(
                "session for device {:?} on {}/{} was refused after opening: {e:#}",
                row.device,
                vault.tenant(),
                vault.domain()
            );
            let vault_for_audit = vault.clone();
            let device = row.device.clone();
            let recorded = meta_op(node, move |_| {
                vault_for_audit.meta().record_auth_event(
                    AuthEventKind::SessionRefused,
                    Some(&device),
                    Some(&pin.to_string()),
                    &why,
                )
            })
            .await;
            if let Err(audit) = recorded {
                tracing::error!("could not record a refused session: {audit:#}");
            }
            return Err(e);
        }
    };
    tracing::info!(
        domain = vault.domain(),
        device = row.device,
        segments = report.segments_completed,
        blocks = report.blocks_landed,
        "receive session complete"
    );
    Ok(())
}

/// Tell the peer why, best effort, then fail — the same posture the session
/// state machine takes (`session::refuse`).
async fn refuse<W>(writer: &mut W, code: &str, message: String) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let _ = write_frame(
        writer,
        &Frame::Error(ProtoError {
            code: code.to_string(),
            message: message.clone(),
        }),
    )
    .await;
    anyhow::bail!("refused ({code}): {message}")
}

#[cfg(test)]
mod tests {
    use super::*;

    use hive_core::identity::DeviceIdentity;

    #[test]
    fn the_accept_rate_allows_a_burst_then_refills_over_time() {
        let start = Instant::now();
        let mut rate = AcceptRate::new(4, 2.0, start);
        for i in 0..4 {
            assert!(rate.allow(start), "burst connection {i}");
        }
        assert!(!rate.allow(start), "the burst is spent");

        // Half a second at 2/s is one token back.
        let later = start + Duration::from_millis(500);
        assert!(rate.allow(later));
        assert!(!rate.allow(later));

        // And it never banks more than the burst.
        let much_later = start + Duration::from_secs(3600);
        for _ in 0..4 {
            assert!(rate.allow(much_later));
        }
        assert!(!rate.allow(much_later));
    }

    #[test]
    fn the_gate_admits_pinned_keys_and_strangers_only_while_enrolling() {
        let laptop = SpkiPin::of(&DeviceIdentity::from_seed(&[1; 32]));
        let stranger = SpkiPin::of(&DeviceIdentity::from_seed(&[2; 32]));

        let closed = NodeGate::new(PinSet::one(laptop), false);
        assert!(closed.accepts(&laptop));
        assert!(!closed.accepts(&stranger), "no code, no strangers");
        assert!(!closed.is_open_door());

        let open = NodeGate::new(PinSet::one(laptop), true);
        assert!(open.accepts(&laptop));
        assert!(
            open.accepts(&stranger),
            "enrollment is how a key becomes known; there is nothing to pin it against yet"
        );
        assert!(open.is_open_door());

        // A node with no devices and no codes is a node nobody can reach.
        assert!(!NodeGate::new(PinSet::default(), false).accepts(&laptop));
    }

    /// The gate reads node-meta, so a revoke takes effect on the next
    /// connection with no restart — the reason `PinnedKeys` is a trait.
    #[test]
    fn the_gate_follows_node_meta_without_a_restart() {
        let root = tempfile::tempdir().unwrap();
        let mut node = Node::open(root.path()).unwrap();
        let vault = node.open_vault("household", "example.com").unwrap();
        let laptop = DeviceIdentity::from_seed(&[1; 32]);
        let pin = SpkiPin::of(&laptop);

        assert!(!node.gate(SystemTime::now()).unwrap().accepts(&pin));
        vault
            .meta()
            .pin_device("dev-laptop", &laptop.ed25519_public(), &[9u8; 32])
            .unwrap();
        assert!(node.gate(SystemTime::now()).unwrap().accepts(&pin));
        assert_eq!(
            node.vault_for_pin(&pin).unwrap().unwrap().1.device,
            "dev-laptop"
        );

        vault.meta().revoke_device("dev-laptop").unwrap();
        assert!(
            !node.gate(SystemTime::now()).unwrap().accepts(&pin),
            "the same node object, no restart, no new listener"
        );
        assert!(node.vault_for_pin(&pin).unwrap().is_none());
    }

    #[test]
    fn a_live_code_opens_the_door_and_expiry_closes_it() {
        let root = tempfile::tempdir().unwrap();
        let mut node = Node::open(root.path()).unwrap();
        let vault = node.open_vault("household", "example.com").unwrap();
        let now = SystemTime::now();
        assert!(!node.gate(now).unwrap().is_open_door());

        let minted = enroll::mint(&vault, now).unwrap();
        assert!(node.gate(now).unwrap().is_open_door());
        assert_eq!(
            node.vault_for_code(&minted.code).unwrap().unwrap().domain(),
            "example.com"
        );
        assert!(node
            .vault_for_code("AAAAAAAAAAAAAAAAAAAAA")
            .unwrap()
            .is_none());

        assert!(
            !node
                .gate(now + enroll::CODE_TTL + Duration::from_secs(1))
                .unwrap()
                .is_open_door(),
            "the window closes on its own"
        );
    }
}
