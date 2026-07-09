use anyhow::Result;
use hive_mail::MailDaemon;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "hive_mail=info,hive_api=info".into()),
        ))
        .with(tracing_subscriber::fmt::layer())
        .init();

    // The compose service always runs (restart: unless-stopped); the flag
    // decides whether it syncs or idles. Idling keeps the service green so
    // enabling mail is an env flip + restart, not a compose edit.
    if !hive_mail::mail_enabled() {
        tracing::info!("HIVE_MAIL_ENABLED is off — hive-mail idling");
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(300)).await;
        }
    }

    // Same open path as api/worker (DATABASE_URL + schema migrate).
    let pool = hive_api::db::init().await?;
    let daemon = MailDaemon::new(pool);
    if std::env::args().any(|a| a == "--once" || a == "once") {
        daemon.run_once().await?;
        tracing::info!("one-shot run done, exiting");
        Ok(())
    } else {
        daemon.run().await
    }
}
