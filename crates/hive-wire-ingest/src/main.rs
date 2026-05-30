//! Poll configured `wire_sources` and insert deduped rows into `wire_events`.
//!
//! Wire is the external input surface (RSS today; scrape + messaging bridge later).
//! Journal prose remains the canonical path for tasks, notes, and links.

use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use hive_db::enums::Severity;
use hive_db::queries::{wire, wire_sources};
use hive_db::{PgPool, default_database_url, open_pool};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "hive-wire-ingest",
    about = "Poll wire_sources and ingest wire_events"
)]
struct Args {
    /// Postgres URL (also: DATABASE_URL env).
    #[arg(long, env = "DATABASE_URL")]
    database_url: Option<String>,

    /// Seconds between poll sweeps (checks all due sources each sweep).
    #[arg(long, env = "WIRE_INGEST_INTERVAL_SECS", default_value_t = 60)]
    interval_secs: u64,

    /// Run one sweep then exit (useful for cron or smoke).
    #[arg(long)]
    once: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env().add_directive("hive_wire_ingest=info".parse()?),
        )
        .init();

    let args = Args::parse();
    let url = args.database_url.unwrap_or_else(default_database_url);
    let pool = open_pool(&url, 2).await.context("open postgres pool")?;

    loop {
        if let Err(e) = sweep(&pool).await {
            tracing::error!(error = %e, "wire ingest sweep failed");
        }
        if args.once {
            break;
        }
        tokio::time::sleep(Duration::from_secs(args.interval_secs)).await;
    }

    Ok(())
}

async fn sweep(pool: &PgPool) -> Result<()> {
    let sources = wire_sources::due_for_poll(pool)
        .await
        .context("list due wire sources")?;
    if sources.is_empty() {
        tracing::debug!("no wire sources due");
        return Ok(());
    }

    for source in sources {
        let result = match source.kind.as_str() {
            "rss" => poll_rss(pool, &source).await,
            other => Err(anyhow::anyhow!("unsupported wire source kind: {other}")),
        };

        match result {
            Ok(()) => {
                wire_sources::mark_fetched(pool, source.id, None)
                    .await
                    .context("mark wire source fetched")?;
            }
            Err(e) => {
                tracing::warn!(source = %source.name, error = %e, "wire source poll failed");
                wire_sources::mark_fetched(pool, source.id, Some(&e.to_string()))
                    .await
                    .context("mark wire source error")?;
            }
        }
    }

    Ok(())
}

async fn poll_rss(pool: &PgPool, source: &wire_sources::WireSource) -> Result<()> {
    let bytes = reqwest::get(&source.url)
        .await
        .with_context(|| format!("fetch rss {}", source.url))?
        .error_for_status()
        .with_context(|| format!("rss HTTP error {}", source.url))?
        .bytes()
        .await
        .context("read rss body")?;

    let channel = rss::Channel::read_from(bytes.as_ref()).context("parse rss")?;
    let default_severity = source
        .default_severity
        .as_deref()
        .and_then(|s| Severity::from_str(s).ok());

    for item in channel.items() {
        let title = item.title().unwrap_or("(untitled)").trim();
        if title.is_empty() {
            continue;
        }
        let external_id = item
            .guid()
            .map(|g| g.value().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| item.link().map(str::to_string))
            .unwrap_or_else(|| title.to_string());

        let body = item.description().map(str::to_string);
        let url = item.link().map(str::to_string);

        let res = wire::add(
            pool,
            wire::AddArgs {
                source: &source.source_tag,
                title,
                body: body.as_deref(),
                external_id: Some(&external_id),
                severity: default_severity,
                affects: source.affects.as_deref(),
                url: url.as_deref(),
                category: source.category.as_deref(),
            },
        )
        .await
        .context("insert wire event")?;

        match res {
            wire::AddResult::Added(e) => {
                tracing::info!(
                    source = %source.name,
                    wire_id = %e.id,
                    title = %e.title,
                    "ingested wire event"
                );
            }
            wire::AddResult::AlreadySeen { id } => {
                tracing::debug!(source = %source.name, wire_id = %id, "wire event already seen");
            }
        }
    }

    Ok(())
}
