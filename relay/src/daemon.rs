//! The relay daemon Nate operates.
//!
//! Two listeners and nothing else:
//!
//! * **ingress** (public, 443) ... browsers arrive here. The daemon reads the
//!   SNI out of the ClientHello, which is plaintext by construction, finds the
//!   house that claimed that name, and splices the connection to it. It holds
//!   no certificate for any instance name and is not a party to the handshake
//!   that follows.
//! * **control** (public) ... houses dial in from behind their NAT and hold a
//!   connection open. Outbound only, so no port forwarding anywhere.
//!
//! # What the operator can and cannot see
//!
//! Can: instance ids, the SNI on each connection, client IP addresses, timings,
//! byte counts. That is the irreducible metadata of any relay.
//!
//! Cannot: requests, responses, headers, journal prose. TLS terminates inside
//! the house on a key the relay has never held, and the forwarded stream goes
//! through [`tokio::io::copy_bidirectional`] without being parsed.
//!
//! The private key never leaving the house is load-bearing and not merely
//! tidy: a relay that ever held one could impersonate that instance later,
//! which would turn "we cannot read it" into "we cannot read it today".

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::control::{line, valid_instance_id, ClientMsg, LineReader, ServerMsg};
use crate::limits::Limits;
use crate::sni::{instance_from_sni, peek_sni, Peek, MAX_HELLO};
use crate::tap::{Splice, Tap};

/// How often the daemon pings a registered house, and therefore how long a
/// dead one keeps its id.
///
/// This is not cosmetic. Registration is exclusive, so an instance whose
/// network dropped is locked out of reconnecting for as long as the daemon
/// believes the zombie socket is alive. Before the pong was read, that was
/// however long TCP retransmission took ... minutes. Now it is two intervals.
const HEARTBEAT: Duration = Duration::from_secs(20);

/// A data connection the agent dialled back on, plus anything it sent past the
/// `Data` line. The residue is normally empty and is carried rather than
/// dropped so that stops being load-bearing.
type DataConn = (TcpStream, Vec<u8>);

pub struct Config {
    /// Public TLS port. Nothing here terminates TLS; it only routes.
    pub ingress_addr: SocketAddr,
    /// Where houses dial in.
    pub control_addr: SocketAddr,
    /// Loopback status endpoint. It lists every connected instance.
    pub admin_addr: SocketAddr,
    /// DNS suffix. `<id>.<zone>` is what an instance's certificate covers.
    pub zone: String,
    /// instance id -> registration token.
    pub tokens: HashMap<String, String>,
    pub limits: Limits,
    /// Verification affordance only. See [`crate::tap`].
    pub audit_tap: Option<Arc<Tap>>,
}

pub struct Instance {
    pub id: String,
    pub label: String,
    pub host: String,
    pub connected_at: chrono::DateTime<chrono::Utc>,
    /// Concurrent sessions. `Arc` because a [`ConnPermit`] outlives the borrow
    /// that made it and decrements this on drop.
    pub live: Arc<AtomicUsize>,
    pub bytes_up: AtomicU64,
    pub bytes_down: AtomicU64,
    control: mpsc::Sender<ServerMsg>,
    pending: Mutex<HashMap<String, oneshot::Sender<DataConn>>>,
}

/// Why a registration was refused. One rejection message goes on the wire for
/// all of them, so the control port is not an instance-id oracle.
enum Refused {
    AtCapacity,
    Duplicate,
}

/// The routing table.
///
/// A `std` lock rather than a `tokio` one, deliberately: every operation is a
/// hash lookup with no await in it, and being synchronous is what lets
/// deregistration live in a `Drop` guard instead of a fallthrough path that
/// four `?` operators were skipping past.
#[derive(Default)]
pub struct Registry {
    by_id: RwLock<HashMap<String, Arc<Instance>>>,
}

impl Registry {
    pub fn get(&self, id: &str) -> Option<Arc<Instance>> {
        self.read().get(id).cloned()
    }

    pub fn list(&self) -> Vec<Arc<Instance>> {
        let mut v: Vec<_> = self.read().values().cloned().collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }

    pub fn count(&self) -> usize {
        self.read().len()
    }

    /// Capacity check, duplicate check, and insert under ONE lock. Split
    /// across three they raced: two agents claiming the same id could both
    /// pass the check before either inserted.
    fn claim(&self, inst: Arc<Instance>, max_instances: usize) -> Result<(), Refused> {
        let mut by_id = self.write();
        if by_id.contains_key(&inst.id) {
            return Err(Refused::Duplicate);
        }
        if by_id.len() >= max_instances {
            return Err(Refused::AtCapacity);
        }
        by_id.insert(inst.id.clone(), inst);
        Ok(())
    }

    /// Remove, but only if the entry is still the one that was claimed. The
    /// identity check means a late drop can never evict a live successor.
    fn release(&self, inst: &Arc<Instance>) -> bool {
        let mut by_id = self.write();
        match by_id.get(&inst.id) {
            Some(current) if Arc::ptr_eq(current, inst) => {
                by_id.remove(&inst.id);
                true
            }
            _ => false,
        }
    }

    // A poisoned registry is not a reason to stop routing: nothing here can
    // leave the map half-updated.
    fn read(&self) -> std::sync::RwLockReadGuard<'_, HashMap<String, Arc<Instance>>> {
        self.by_id.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<String, Arc<Instance>>> {
        self.by_id.write().unwrap_or_else(|e| e.into_inner())
    }
}

/// Deregisters on drop, which is the only way to be sure.
///
/// Registration used to be undone by code at the bottom of `register`, past
/// four `?` operators that could return early. One of them was the welcome
/// write: a peer that reset between the insert and that write left an entry
/// pointing at a dead control channel, and because registration is exclusive,
/// that house could never reconnect until the relay restarted.
struct Registration {
    daemon: Arc<Daemon>,
    inst: Arc<Instance>,
}

impl Drop for Registration {
    fn drop(&mut self) {
        if self.daemon.registry.release(&self.inst) {
            // The off switch taking effect: nothing routes here now.
            tracing::info!(instance = %self.inst.id, "instance disconnected");
        }
    }
}

pub struct Daemon {
    pub cfg: Config,
    pub registry: Registry,
    pub limits_state: crate::limits::State,
}

impl Daemon {
    pub fn new(cfg: Config) -> Arc<Self> {
        Arc::new(Self {
            limits_state: crate::limits::State::new(cfg.limits.clone()),
            cfg,
            registry: Registry::default(),
        })
    }

    // ---- ingress: browsers -------------------------------------------------

    pub async fn run_ingress(self: Arc<Self>) -> Result<()> {
        let listener = TcpListener::bind(self.cfg.ingress_addr)
            .await
            .with_context(|| format!("bind ingress {}", self.cfg.ingress_addr))?;
        tracing::info!(addr = %self.cfg.ingress_addr, "ingress listening (TLS passthrough)");
        loop {
            let (sock, peer) = match listener.accept().await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error = %e, "ingress accept failed");
                    continue;
                }
            };
            let me = self.clone();
            tokio::spawn(async move {
                if let Err(e) = me.route(sock, peer).await {
                    tracing::debug!(%peer, error = %e, "ingress connection ended");
                }
            });
        }
    }

    async fn route(self: Arc<Self>, mut client: TcpStream, peer: SocketAddr) -> Result<()> {
        // The global ceiling comes first and is held for the whole session,
        // splice included. A rate limit alone bounds arrivals, not residents.
        let Some(_slot) = self.limits_state.ingress_slot() else {
            tracing::warn!(%peer, "ingress at global capacity");
            return Ok(());
        };
        if let Err(e) = self.limits_state.check_ingress(peer.ip(), Instant::now()) {
            // No response. This port speaks TLS and we are below it, so there
            // is nothing meaningful to say ... close and move on.
            tracing::warn!(%peer, reason = %e, "ingress refused");
            return Ok(());
        }
        client.set_nodelay(true).ok();

        // ONE deadline for the whole read, not one per read. Per-read, a byte
        // every nine seconds holds the connection for as long as the attacker
        // has patience.
        let deadline = tokio::time::Instant::now() + self.cfg.limits.handshake_timeout;

        // Read only as far as the ClientHello, then stop looking.
        let mut hello = Vec::with_capacity(1024);
        let sni = loop {
            if hello.len() >= MAX_HELLO {
                bail!("no ClientHello within {MAX_HELLO} bytes");
            }
            let mut chunk = [0u8; 2048];
            let n = tokio::time::timeout_at(deadline, client.read(&mut chunk))
                .await
                .context("timed out waiting for a ClientHello")??;
            if n == 0 {
                bail!("closed before sending a ClientHello");
            }
            hello.extend_from_slice(&chunk[..n]);
            match peek_sni(&hello) {
                Ok(Peek::Name(name)) => break name,
                Ok(Peek::Incomplete) => continue,
                // Decided, not truncated: waiting for more bytes would hold a
                // slot for the whole deadline and still end here.
                Ok(Peek::Nameless) => bail!("ClientHello carried no server_name"),
                Err(()) => bail!("not a TLS ClientHello"),
            }
        };

        let Some(id) = instance_from_sni(&sni, &self.cfg.zone) else {
            bail!("SNI {sni} is not in zone {}", self.cfg.zone);
        };
        let Some(inst) = self.registry.get(&id) else {
            // Closing without a TLS alert is indistinguishable from a network
            // drop, so this does not confirm whether the id exists.
            bail!("no instance connected for {id}");
        };

        self.bridge(client, peer, inst, hello).await
    }

    async fn bridge(
        self: Arc<Self>,
        client: TcpStream,
        peer: SocketAddr,
        inst: Arc<Instance>,
        hello: Vec<u8>,
    ) -> Result<()> {
        let live = inst.live.fetch_add(1, Ordering::SeqCst) + 1;
        if live > self.cfg.limits.max_conns_per_instance {
            inst.live.fetch_sub(1, Ordering::SeqCst);
            tracing::warn!(instance = %inst.id, %peer, "instance connection cap reached");
            return Ok(());
        }
        let _permit = ConnPermit {
            live: inst.live.clone(),
        };

        let nonce = nanoid::nanoid!(21);
        let (tx, rx) = oneshot::channel::<DataConn>();
        inst.pending.lock().await.insert(nonce.clone(), tx);

        // Timed, because the control channel is bounded and a house that has
        // stopped reading its socket would otherwise park this task, its
        // permit, and its slot for as long as the process lives.
        let told = tokio::time::timeout(
            self.cfg.limits.handshake_timeout,
            inst.control.send(ServerMsg::Open {
                nonce: nonce.clone(),
            }),
        )
        .await;
        if !matches!(told, Ok(Ok(()))) {
            inst.pending.lock().await.remove(&nonce);
            bail!("instance control channel gone or blocked");
        }

        let (upstream, residue) =
            match tokio::time::timeout(self.cfg.limits.handshake_timeout, rx).await {
                Ok(Ok(s)) => s,
                _ => {
                    inst.pending.lock().await.remove(&nonce);
                    bail!("instance did not dial back in time");
                }
            };
        upstream.set_nodelay(true).ok();

        // The peeked bytes are replayed verbatim, so the instance sees the
        // browser's original ClientHello and the handshake is between them.
        let (up, down) = crate::tap::splice(
            client,
            upstream,
            Splice {
                to_instance: &hello,
                to_client: &residue,
                idle_timeout: Some(self.cfg.limits.idle_timeout),
                tap: self.cfg.audit_tap.clone(),
            },
        )
        .await?;

        inst.bytes_up.fetch_add(up, Ordering::Relaxed);
        inst.bytes_down.fetch_add(down, Ordering::Relaxed);
        tracing::info!(instance = %inst.id, %peer, up, down, "session closed");
        Ok(())
    }

    // ---- control: houses ---------------------------------------------------

    pub async fn run_control(self: Arc<Self>) -> Result<()> {
        let listener = TcpListener::bind(self.cfg.control_addr)
            .await
            .with_context(|| format!("bind control {}", self.cfg.control_addr))?;
        tracing::info!(addr = %self.cfg.control_addr, zone = %self.cfg.zone, "control listening");
        loop {
            let (sock, peer) = match listener.accept().await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error = %e, "control accept failed");
                    continue;
                }
            };
            let me = self.clone();
            tokio::spawn(async move {
                if let Err(e) = me.handle_control(sock, peer).await {
                    tracing::debug!(%peer, error = %e, "control connection ended");
                }
            });
        }
    }

    async fn handle_control(self: Arc<Self>, sock: TcpStream, peer: SocketAddr) -> Result<()> {
        // Same two-part shape as ingress. The control port is public and
        // unauthenticated until the first line parses, so it gets the same
        // ceiling and the same rate limit ... it had neither.
        let Some(_slot) = self.limits_state.control_slot() else {
            tracing::warn!(%peer, "control at global capacity");
            return Ok(());
        };
        if let Err(e) = self.limits_state.check_control(peer.ip(), Instant::now()) {
            tracing::warn!(%peer, reason = %e, "control refused");
            return Ok(());
        }
        sock.set_nodelay(true).ok();

        let mut lines = LineReader::new(sock);
        let first = tokio::time::timeout(self.cfg.limits.handshake_timeout, lines.next_line())
            .await
            .context("timed out waiting for the opening line")??;
        let Some(first) = first else {
            bail!("peer closed before saying anything");
        };

        match serde_json::from_str::<ClientMsg>(first.trim())
            .context("opening line was not a control message")?
        {
            ClientMsg::Hello {
                instance,
                token,
                label,
            } => self.register(lines, peer, instance, token, label).await,
            ClientMsg::Data { nonce } => {
                let (sock, residue) = lines.into_parts();
                self.pair_data(sock, residue, nonce).await
            }
            ClientMsg::Pong => bail!("pong is not an opening message"),
        }
    }

    async fn register(
        self: Arc<Self>,
        mut lines: LineReader<TcpStream>,
        peer: SocketAddr,
        id: String,
        token: String,
        label: Option<String>,
    ) -> Result<()> {
        async fn reject(sock: &mut TcpStream, msg: &str) {
            if let Ok(l) = line(&ServerMsg::Error {
                msg: msg.to_string(),
            }) {
                let _ = sock.write_all(l.as_bytes()).await;
            }
            let _ = sock.shutdown().await;
        }

        if !valid_instance_id(&id) {
            reject(lines.get_mut(), "registration refused").await;
            bail!("invalid instance id from {peer}");
        }
        if !self.check_token(&id, &token) {
            // One message for "unknown id" and "wrong token" so the control
            // port is not an instance-id oracle.
            reject(lines.get_mut(), "registration refused").await;
            bail!("registration refused for {id} from {peer}");
        }

        let host = format!("{id}.{}", self.cfg.zone);
        let (tx, mut rx) = mpsc::channel::<ServerMsg>(64);
        let inst = Arc::new(Instance {
            id: id.clone(),
            label: label.unwrap_or_else(|| id.clone()),
            host: host.clone(),
            connected_at: chrono::Utc::now(),
            live: Arc::new(AtomicUsize::new(0)),
            bytes_up: AtomicU64::new(0),
            bytes_down: AtomicU64::new(0),
            control: tx,
            pending: Mutex::new(HashMap::new()),
        });

        match self
            .registry
            .claim(inst.clone(), self.cfg.limits.max_instances)
        {
            Ok(()) => {}
            Err(Refused::AtCapacity) => {
                reject(lines.get_mut(), "relay at capacity").await;
                bail!("at capacity, refused {id}");
            }
            Err(Refused::Duplicate) => {
                reject(lines.get_mut(), "instance already connected").await;
                bail!("duplicate registration for {id}");
            }
        }

        // From here on, every exit path deregisters ... including the `?` on
        // the welcome write immediately below, which was the leak.
        let _registered = Registration {
            daemon: self.clone(),
            inst: inst.clone(),
        };
        tracing::info!(instance = %id, %host, %peer, "instance registered");

        let welcome = line(&ServerMsg::Welcome { host })?;
        lines.get_mut().write_all(welcome.as_bytes()).await?;

        let mut heartbeat = tokio::time::interval(HEARTBEAT);
        // Delay rather than burst: a runner that stalled must not fire two
        // ticks back to back and read that as a missed pong.
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        heartbeat.tick().await;
        // The heartbeat is a question, so somebody has to check for an answer.
        let mut awaiting_pong = false;
        loop {
            tokio::select! {
                msg = rx.recv() => {
                    let Some(msg) = msg else { return Ok(()) };
                    let l = line(&msg)?;
                    lines.get_mut().write_all(l.as_bytes()).await?;
                }
                _ = heartbeat.tick() => {
                    if awaiting_pong {
                        bail!("no pong within {HEARTBEAT:?}");
                    }
                    let l = line(&ServerMsg::Ping)?;
                    lines.get_mut().write_all(l.as_bytes()).await?;
                    awaiting_pong = true;
                }
                // Cancel safe: a partial line stays in the reader's buffer.
                incoming = lines.next_line() => {
                    let Some(incoming) = incoming? else { return Ok(()) };
                    match serde_json::from_str::<ClientMsg>(incoming.trim())
                        .context("control message from the instance was unparseable")?
                    {
                        ClientMsg::Pong => awaiting_pong = false,
                        ClientMsg::Hello { .. } => bail!("hello twice on one control connection"),
                        ClientMsg::Data { .. } => bail!("data is not a control-session message"),
                    }
                }
            }
        }
    }

    fn check_token(&self, id: &str, presented: &str) -> bool {
        use subtle::ConstantTimeEq;
        let Some(expected) = self.cfg.tokens.get(id) else {
            // Compare anyway so an unknown id and a bad token cost the same.
            let _: bool = presented.as_bytes().ct_eq(presented.as_bytes()).into();
            return false;
        };
        expected.as_bytes().ct_eq(presented.as_bytes()).into()
    }

    async fn pair_data(&self, sock: TcpStream, residue: Vec<u8>, nonce: String) -> Result<()> {
        // Nonces are single-use, instance-scoped, and live as long as the
        // handshake timeout, so a guess has to hit a 21-character nanoid
        // inside that window.
        for inst in self.registry.list() {
            let taken = inst.pending.lock().await.remove(&nonce);
            if let Some(tx) = taken {
                return tx
                    .send((sock, residue))
                    .map_err(|_| anyhow::anyhow!("waiting client vanished"));
            }
        }
        bail!("unknown or expired nonce")
    }
}

/// Decrements an instance's live-session count on drop.
pub struct ConnPermit {
    live: Arc<AtomicUsize>,
}

impl Drop for ConnPermit {
    fn drop(&mut self) {
        self.live.fetch_sub(1, Ordering::SeqCst);
    }
}
