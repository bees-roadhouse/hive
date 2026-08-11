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
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, Mutex, RwLock};

use crate::control::{line, valid_instance_id, ClientMsg, ServerMsg};
use crate::limits::Limits;
use crate::sni::{instance_from_sni, peek_sni, MAX_HELLO};
use crate::tap::Tap;

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
    pending: Mutex<HashMap<String, oneshot::Sender<TcpStream>>>,
}

impl Instance {
    /// Registry-free constructor for tests.
    #[doc(hidden)]
    pub fn for_test(id: String, host: String) -> Self {
        let (control, _rx) = mpsc::channel(1);
        Self {
            label: id.clone(),
            id,
            host,
            connected_at: chrono::Utc::now(),
            live: Arc::new(AtomicUsize::new(0)),
            bytes_up: AtomicU64::new(0),
            bytes_down: AtomicU64::new(0),
            control,
            pending: Mutex::new(HashMap::new()),
        }
    }
}

#[derive(Default)]
pub struct Registry {
    by_id: RwLock<HashMap<String, Arc<Instance>>>,
}

impl Registry {
    pub async fn get(&self, id: &str) -> Option<Arc<Instance>> {
        self.by_id.read().await.get(id).cloned()
    }

    pub async fn list(&self) -> Vec<Arc<Instance>> {
        let mut v: Vec<_> = self.by_id.read().await.values().cloned().collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }

    pub async fn count(&self) -> usize {
        self.by_id.read().await.len()
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
        if let Err(e) = self.limits_state.check_rate(peer.ip(), Instant::now()) {
            // No response. This port speaks TLS and we are below it, so there
            // is nothing meaningful to say ... close and move on.
            tracing::warn!(%peer, reason = %e, "ingress refused");
            return Ok(());
        }
        client.set_nodelay(true).ok();

        // Read only as far as the ClientHello, then stop looking.
        let mut hello = Vec::with_capacity(1024);
        let sni = loop {
            if hello.len() > MAX_HELLO {
                bail!("no ClientHello within {MAX_HELLO} bytes");
            }
            let mut chunk = [0u8; 2048];
            let n = tokio::time::timeout(Duration::from_secs(10), client.read(&mut chunk))
                .await
                .context("timed out waiting for a ClientHello")??;
            if n == 0 {
                bail!("closed before sending a ClientHello");
            }
            hello.extend_from_slice(&chunk[..n]);
            match peek_sni(&hello) {
                Ok(Some(name)) => break name,
                Ok(None) => continue,
                Err(()) => bail!("not a TLS ClientHello"),
            }
        };

        let Some(id) = instance_from_sni(&sni, &self.cfg.zone) else {
            bail!("SNI {sni} is not in zone {}", self.cfg.zone);
        };
        let Some(inst) = self.registry.get(&id).await else {
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
        let (tx, rx) = oneshot::channel::<TcpStream>();
        inst.pending.lock().await.insert(nonce.clone(), tx);

        if inst
            .control
            .send(ServerMsg::Open {
                nonce: nonce.clone(),
            })
            .await
            .is_err()
        {
            inst.pending.lock().await.remove(&nonce);
            bail!("instance control channel gone");
        }

        let upstream = match tokio::time::timeout(Duration::from_secs(10), rx).await {
            Ok(Ok(s)) => s,
            _ => {
                inst.pending.lock().await.remove(&nonce);
                bail!("instance did not dial back in time");
            }
        };
        upstream.set_nodelay(true).ok();

        // The peeked bytes are replayed verbatim, so the instance sees the
        // browser's original ClientHello and the handshake is between them.
        let (up, down) =
            crate::tap::splice(client, upstream, &hello, self.cfg.audit_tap.clone()).await?;

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
        sock.set_nodelay(true).ok();
        let mut reader = BufReader::new(sock);
        let mut first = String::new();
        let read = tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut first))
            .await
            .context("timed out waiting for the opening line")??;
        if read == 0 {
            bail!("peer closed before saying anything");
        }
        if first.len() > 8 * 1024 {
            bail!("opening line too long");
        }

        match serde_json::from_str::<ClientMsg>(first.trim())
            .context("opening line was not a control message")?
        {
            ClientMsg::Hello {
                instance,
                token,
                label,
            } => self.register(reader, peer, instance, token, label).await,
            ClientMsg::Data { nonce } => self.pair_data(reader.into_inner(), nonce).await,
            ClientMsg::Pong => bail!("pong is not an opening message"),
        }
    }

    async fn register(
        self: Arc<Self>,
        mut reader: BufReader<TcpStream>,
        peer: SocketAddr,
        id: String,
        token: String,
        label: Option<String>,
    ) -> Result<()> {
        async fn reject(mut reader: BufReader<TcpStream>, msg: &str) {
            if let Ok(l) = line(&ServerMsg::Error {
                msg: msg.to_string(),
            }) {
                let _ = reader.get_mut().write_all(l.as_bytes()).await;
            }
            let _ = reader.get_mut().shutdown().await;
        }

        if !valid_instance_id(&id) {
            reject(reader, "registration refused").await;
            bail!("invalid instance id from {peer}");
        }
        if !self.check_token(&id, &token) {
            // One message for "unknown id" and "wrong token" so the control
            // port is not an instance-id oracle.
            reject(reader, "registration refused").await;
            bail!("registration refused for {id} from {peer}");
        }
        if self.registry.count().await >= self.cfg.limits.max_instances {
            reject(reader, "relay at capacity").await;
            bail!("at capacity, refused {id}");
        }
        if self.registry.get(&id).await.is_some() {
            reject(reader, "instance already connected").await;
            bail!("duplicate registration for {id}");
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

        self.registry
            .by_id
            .write()
            .await
            .insert(id.clone(), inst.clone());
        tracing::info!(instance = %id, %host, %peer, "instance registered");

        let welcome = line(&ServerMsg::Welcome { host })?;
        reader.get_mut().write_all(welcome.as_bytes()).await?;

        let mut heartbeat = tokio::time::interval(Duration::from_secs(20));
        heartbeat.tick().await;
        let mut incoming = String::new();
        let result: Result<()> = loop {
            tokio::select! {
                msg = rx.recv() => {
                    let Some(msg) = msg else { break Ok(()) };
                    let l = line(&msg)?;
                    if let Err(e) = reader.get_mut().write_all(l.as_bytes()).await {
                        break Err(e.into());
                    }
                }
                _ = heartbeat.tick() => {
                    let l = line(&ServerMsg::Ping)?;
                    if let Err(e) = reader.get_mut().write_all(l.as_bytes()).await {
                        break Err(e.into());
                    }
                }
                n = reader.read_line(&mut incoming) => {
                    match n {
                        Ok(0) => break Ok(()),
                        Ok(_) => incoming.clear(),
                        Err(e) => break Err(e.into()),
                    }
                }
            }
        };

        // Deregistration is the off switch taking effect: the moment the house
        // stops holding this connection, nothing routes to it.
        self.registry.by_id.write().await.remove(&id);
        tracing::info!(instance = %id, "instance disconnected");
        result
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

    async fn pair_data(&self, sock: TcpStream, nonce: String) -> Result<()> {
        // Nonces are single-use, instance-scoped, and live for 10 seconds, so
        // a guess has to hit a 21-character nanoid inside that window.
        for inst in self.registry.list().await {
            let taken = inst.pending.lock().await.remove(&nonce);
            if let Some(tx) = taken {
                return tx
                    .send(sock)
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

/// Unused-import guard: `IpAddr` is referenced only through `peer.ip()`.
const _: fn(IpAddr) = |_| {};
