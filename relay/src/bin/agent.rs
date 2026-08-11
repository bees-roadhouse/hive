//! The household side. Runs next to hive-api and dials out to the relay.
//!
//! Remote access is OFF unless this process is running and
//! `HIVE_RELAY_ENABLED=1` is set. Both are deliberate: a self-hoster who does
//! not want anyone else's infrastructure in the path simply never starts it,
//! and a stray config file cannot switch it on by accident.

use anyhow::Result;
use hive_relay::agent::{run, AgentConfig};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn require(key: &str) -> Result<String> {
    std::env::var(key).map_err(|_| anyhow::anyhow!("{key} is required"))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(env_or(
            "RUST_LOG",
            "hive_relay=info",
        )))
        .with(tracing_subscriber::fmt::layer())
        .init();

    // The switch. Explicit, single-purpose, and checked before anything dials.
    if env_or("HIVE_RELAY_ENABLED", "0") != "1" {
        eprintln!(
            "remote access is off.\n\
             \n\
             This instance is reachable on loopback only. To publish it through\n\
             a relay, set HIVE_RELAY_ENABLED=1 along with HIVE_RELAY_URL,\n\
             HIVE_RELAY_INSTANCE, HIVE_RELAY_TOKEN, HIVE_RELAY_CERT and\n\
             HIVE_RELAY_KEY. Stopping this process turns remote access off\n\
             again immediately."
        );
        std::process::exit(0);
    }

    // rustls 0.23 wants an explicit process-wide provider.
    tokio_rustls::rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("a rustls crypto provider was already installed"))?;

    let cfg = AgentConfig {
        relay: require("HIVE_RELAY_URL")?,
        instance: require("HIVE_RELAY_INSTANCE")?,
        token: require("HIVE_RELAY_TOKEN")?,
        target: env_or("HIVE_RELAY_TARGET", "127.0.0.1:7878"),
        cert: require("HIVE_RELAY_CERT")?.into(),
        key: require("HIVE_RELAY_KEY")?.into(),
        label: std::env::var("HIVE_RELAY_LABEL").ok(),
        plaintext_tap: match std::env::var("HIVE_RELAY_PLAINTEXT_TAP") {
            Ok(path) if !path.is_empty() => {
                let tap = hive_relay::tap::Tap::create(&path).await?;
                tracing::warn!(
                    path = %tap.path().display(),
                    "PLAINTEXT TAP ON: the decrypted stream is being written to \
                     disk. This exists to prove what the relay cannot see and \
                     must never be enabled outside a demo."
                );
                Some(tap)
            }
            _ => None,
        },
    };

    tracing::info!(
        relay = %cfg.relay, instance = %cfg.instance, target = %cfg.target,
        "remote access ON"
    );
    run(cfg).await
}
