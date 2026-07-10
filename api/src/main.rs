use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

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
    // Fold any legacy people.bio/role into the canonical profile card (idempotent).
    store.backfill_identity_cards().await?;

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
