//! The household side: dials out, terminates TLS, hands plaintext to hive-api.
//!
//! This is what makes the whole arrangement need no port forwarding. Every
//! connection is outbound, so a home router's default deny-inbound stance is
//! never in the way and CGNAT does not matter.
//!
//! It is also where the trust boundary sits. The certificate for
//! `<id>.<zone>` and its private key live here. The relay never holds either,
//! which is why the relay cannot read what it forwards ... and why a compromise
//! of the relay does not become a compromise of the journal.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;

use crate::control::{line, ClientMsg, ServerMsg};

#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// `host:port` of the relay's control listener.
    pub relay: String,
    pub instance: String,
    pub token: String,
    /// Where hive-api is listening, on loopback.
    pub target: String,
    pub cert: PathBuf,
    pub key: PathBuf,
    pub label: Option<String>,
    /// Records the decrypted stream, for comparing against what the relay saw
    /// of the same session. Verification only.
    pub plaintext_tap: Option<Arc<crate::tap::Tap>>,
}

pub fn load_tls(cert: &Path, key: &Path) -> Result<TlsAcceptor> {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert)
        .with_context(|| format!("read certificate {}", cert.display()))?
        .collect::<Result<Vec<_>, _>>()
        .context("parse certificate chain")?;
    if certs.is_empty() {
        bail!("no certificates in {}", cert.display());
    }
    let key = PrivateKeyDer::from_pem_file(key)
        .with_context(|| format!("read private key {}", key.display()))?;

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("build TLS config")?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// Connect, serve, and reconnect forever with a capped backoff.
pub async fn run(cfg: AgentConfig) -> Result<()> {
    let tls = load_tls(&cfg.cert, &cfg.key)?;
    let mut backoff = Duration::from_secs(1);
    loop {
        match session(&cfg, tls.clone()).await {
            Ok(()) => {
                tracing::warn!("relay closed the control connection, reconnecting");
                backoff = Duration::from_secs(1);
            }
            Err(e) => {
                tracing::warn!(error = %e, retry_in = ?backoff, "relay session failed");
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

async fn session(cfg: &AgentConfig, tls: TlsAcceptor) -> Result<()> {
    let sock = TcpStream::connect(&cfg.relay)
        .await
        .with_context(|| format!("dial relay {}", cfg.relay))?;
    sock.set_nodelay(true).ok();
    let mut ctrl = BufReader::new(sock);

    let hello = line(&ClientMsg::Hello {
        instance: cfg.instance.clone(),
        token: cfg.token.clone(),
        label: cfg.label.clone(),
    })?;
    ctrl.get_mut().write_all(hello.as_bytes()).await?;

    let mut buf = String::new();
    loop {
        buf.clear();
        if ctrl.read_line(&mut buf).await? == 0 {
            return Ok(());
        }
        match serde_json::from_str::<ServerMsg>(buf.trim())
            .with_context(|| format!("relay sent something unparseable: {}", buf.trim()))?
        {
            ServerMsg::Welcome { host } => {
                tracing::info!(
                    %host, target = %cfg.target,
                    "registered; the relay forwards ciphertext for this name and holds no key for it"
                );
            }
            ServerMsg::Open { nonce } => {
                let cfg = cfg.clone();
                let tls = tls.clone();
                tokio::spawn(async move {
                    if let Err(e) = serve_one(&cfg, tls, nonce).await {
                        tracing::warn!(error = %e, "session failed");
                    }
                });
            }
            ServerMsg::Ping => {
                let pong = line(&ClientMsg::Pong)?;
                ctrl.get_mut().write_all(pong.as_bytes()).await?;
            }
            ServerMsg::Error { msg } => bail!("relay refused: {msg}"),
        }
    }
}

/// Dial back for one paired client, complete the TLS handshake the browser
/// started, and pipe the decrypted stream to hive-api.
async fn serve_one(cfg: &AgentConfig, tls: TlsAcceptor, nonce: String) -> Result<()> {
    let mut sock = TcpStream::connect(&cfg.relay).await?;
    sock.set_nodelay(true).ok();
    sock.write_all(line(&ClientMsg::Data { nonce })?.as_bytes())
        .await?;

    // The handshake here is with the BROWSER, not with the relay. The relay is
    // copying these bytes without being able to interpret them.
    let tls_stream = tls.accept(sock).await.context("TLS handshake")?;

    let upstream = TcpStream::connect(&cfg.target)
        .await
        .with_context(|| format!("connect hive-api at {}", cfg.target))?;
    upstream.set_nodelay(true).ok();

    // `plaintext_tap` records the DECRYPTED stream. It exists to be diffed
    // against the relay's capture of the same session: same bytes, one side
    // readable and one side not. Demo affordance, never a shipping default.
    let (up, down) =
        crate::tap::splice(tls_stream, upstream, &[], cfg.plaintext_tap.clone()).await?;
    tracing::debug!(up, down, "session closed");
    Ok(())
}
