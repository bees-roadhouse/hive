use std::time::Duration;

use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

/// How often to reconcile stored artifact bytes against the `artifacts` table.
/// `$HIVE_ARTIFACT_SWEEP_SECS`; 0 turns the sweeper off.
const DEFAULT_SWEEP_SECS: u64 = 6 * 60 * 60;

/// How stale a partial upload or an unreferenced object has to look before the
/// sweeper touches it. The blob lock already keeps it off anything in flight;
/// this covers the one case a lock cannot, a request still streaming its body.
const SWEEP_MIN_AGE: Duration = Duration::from_secs(60 * 60);

/// Nothing else collects what the artifact layer can leave behind: partial
/// uploads from dropped connections, bytes a delete moved aside and did not
/// finish disposing of, and objects an upload landed before failing to commit
/// its row. The first pass runs at boot, which is when the previous process's
/// crash litter is sitting there.
fn spawn_artifact_sweeper(store: hive_api::store::Store) {
    let every = std::env::var("HIVE_ARTIFACT_SWEEP_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_SWEEP_SECS);
    if every == 0 {
        tracing::info!("artifact sweeper disabled (HIVE_ARTIFACT_SWEEP_SECS=0)");
        return;
    }
    tokio::spawn(async move {
        let storage = hive_core::artifact_storage::storage();
        let mut ticker = tokio::time::interval(Duration::from_secs(every));
        loop {
            ticker.tick().await;
            match store.artifacts_sweep_all(storage, SWEEP_MIN_AGE).await {
                Ok(swept) if swept.is_empty() => {}
                Ok(swept) => tracing::info!(
                    temps_removed = swept.temps_removed,
                    trash_restored = swept.trash_restored,
                    trash_purged = swept.trash_purged,
                    orphans_removed = swept.orphans_removed,
                    "artifact sweep"
                ),
                Err(e) => tracing::warn!(error = %e, "artifact sweep failed"),
            }
        }
    });
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "hive_api=info,hive_core=info,tower_http=info".into()),
        ))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let pool = hive_api::db::init().await?;

    let store = hive_api::store::Store::new(pool);
    // Fold any legacy people.bio/role into the canonical profile card
    // (idempotent). Per-org: `people` is row-secured and boot holds no scope.
    store.backfill_identity_cards_all().await?;

    spawn_artifact_sweeper(store.clone());

    let app = hive_api::routes::router(store);

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(7878);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    tracing::info!(url = %format!("http://localhost:{port}"), mcp = "/mcp", "listening");
    axum::serve(listener, app).await?;
    Ok(())
}
