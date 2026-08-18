//! The relay daemon Nate operates.
//!
//! ```text
//! HIVE_RELAY_ZONE=relay.beesroadhouse.com \
//! HIVE_RELAY_TOKENS=hv7bqk2m9x=<token> \
//! hive-relay
//! ```

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use hive_relay::daemon::{Config, Daemon};
use hive_relay::limits::Limits;
use hive_relay::tap::Tap;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// `id=token,id=token`. A file-backed or database-backed registry is the real
/// answer; this keeps the spike to one moving part.
fn parse_tokens(raw: &str) -> HashMap<String, String> {
    raw.split(',')
        .filter_map(|pair| pair.split_once('='))
        .map(|(id, tok)| (id.trim().to_string(), tok.trim().to_string()))
        .filter(|(id, tok)| !id.is_empty() && !tok.is_empty())
        .collect()
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

    let tokens = parse_tokens(&env_or("HIVE_RELAY_TOKENS", ""));
    if tokens.is_empty() {
        anyhow::bail!("HIVE_RELAY_TOKENS is empty: nothing could register");
    }

    let audit_tap = match std::env::var("HIVE_RELAY_AUDIT_TAP") {
        Ok(path) if !path.is_empty() => {
            let tap = Tap::create(&path).await.context("open audit tap")?;
            tracing::warn!(
                path = %tap.path().display(),
                "AUDIT TAP ON: every forwarded byte is being written to disk. \
                 It is ciphertext the relay cannot read, but this is a \
                 verification affordance and must be off in production."
            );
            Some(tap)
        }
        _ => None,
    };

    let cfg = Config {
        ingress_addr: env_or("HIVE_RELAY_INGRESS", "0.0.0.0:8443").parse()?,
        control_addr: env_or("HIVE_RELAY_CONTROL", "0.0.0.0:7500").parse()?,
        admin_addr: env_or("HIVE_RELAY_ADMIN", "127.0.0.1:7501").parse()?,
        zone: env_or("HIVE_RELAY_ZONE", "relay.localtest.me"),
        tokens,
        limits: Limits::default(),
        audit_tap,
    };
    let admin_addr = cfg.admin_addr;
    let daemon = Daemon::new(cfg);

    let admin = Router::new()
        .route("/status", get(status))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(daemon.clone());
    let listener = tokio::net::TcpListener::bind(admin_addr).await?;
    tracing::info!(%admin_addr, "admin listening");
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, admin).await {
            tracing::error!(error = %e, "admin server stopped");
        }
    });

    let ingress = daemon.clone().run_ingress();
    let control = daemon.run_control();
    tokio::try_join!(ingress, control)?;
    Ok(())
}

/// Everything the relay knows about an instance ... which is the point of
/// publishing it: this is the complete list of what the operator can see.
async fn status(State(daemon): State<Arc<Daemon>>) -> impl IntoResponse {
    let rows: Vec<_> = daemon
        .registry
        .list()
        .iter()
        .map(|i| {
            serde_json::json!({
                "id": i.id,
                "label": i.label,
                "host": i.host,
                "connected_at": i.connected_at.to_rfc3339(),
                "live_sessions": i.live.load(Ordering::Relaxed),
                "bytes_client_to_instance": i.bytes_up.load(Ordering::Relaxed),
                "bytes_instance_to_client": i.bytes_down.load(Ordering::Relaxed),
            })
        })
        .collect();
    Json(serde_json::json!({ "instances": rows }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_token_pairs() {
        let t = parse_tokens("a=1,b=2");
        assert_eq!(t.get("a"), Some(&"1".to_string()));
        assert_eq!(t.get("b"), Some(&"2".to_string()));
    }

    #[test]
    fn ignores_malformed_entries() {
        let t = parse_tokens("a=1,,garbage,=2,c=");
        assert_eq!(t.len(), 1);
        assert!(t.contains_key("a"));
    }
}
