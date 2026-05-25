//! One-shot copy from ~/.hive/hive.db (sqlite) into a fresh postgres.
//!
//! Reads every row from each known table, inserts into postgres with the
//! original id preserved, then advances each `_id_seq` past max(id) so
//! subsequent inserts pick fresh ids. Default is skip-existing on PK clash.

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use clap::Parser;
use pgvector::Vector;
use rusqlite::{Connection as SqliteConn, OpenFlags};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use std::path::PathBuf;

/// Copy ~/.hive/hive.db into a fresh postgres. Idempotent on PK clash.
#[derive(Parser, Debug)]
#[command(name = "hive-migrate", about, long_about = None)]
struct Args {
    /// Source sqlite path (also: HIVE_SQLITE env).
    #[arg(long, env = "HIVE_SQLITE")]
    from: PathBuf,

    /// Target postgres URL (also: DATABASE_URL env).
    #[arg(long, env = "DATABASE_URL")]
    to: String,

    /// Skip rows whose PK already exists in postgres. Default true.
    #[arg(long, default_value_t = true)]
    skip_existing: bool,

    /// Read sqlite + report counts; no postgres writes.
    #[arg(long, default_value_t = false)]
    dry_run: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();
    tracing::info!(from = %args.from.display(), dry_run = args.dry_run, "starting");

    let sqlite = SqliteConn::open_with_flags(&args.from, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open sqlite {}", args.from.display()))?;

    if args.dry_run {
        return dry_run_counts(&sqlite);
    }

    let pg = PgPoolOptions::new()
        .max_connections(4)
        .connect(&args.to)
        .await
        .context("connect postgres")?;

    let mut report: Vec<(&'static str, usize, usize)> = Vec::new();

    report.push(migrate_projects(&sqlite, &pg, args.skip_existing).await?);
    report.push(migrate_tasks(&sqlite, &pg, args.skip_existing).await?);
    report.push(migrate_journal(&sqlite, &pg, args.skip_existing).await?);
    report.push(migrate_notes(&sqlite, &pg, args.skip_existing).await?);
    report.push(migrate_wire(&sqlite, &pg, args.skip_existing).await?);
    report.push(migrate_messages(&sqlite, &pg, args.skip_existing).await?);
    report.push(migrate_links(&sqlite, &pg, args.skip_existing).await?);
    report.push(migrate_embeddings(&sqlite, &pg, args.skip_existing).await?);

    println!("\n=== migration report ===");
    let mut mismatches = 0;
    for (table, src, dst) in &report {
        let tag = if src == dst { "ok" } else { "MISMATCH" };
        println!("  {table:20} src={src:>6} dst={dst:>6}  {tag}");
        if src != dst {
            mismatches += 1;
        }
    }
    if mismatches > 0 {
        return Err(anyhow!("{mismatches} table(s) row-count mismatch"));
    }
    println!("all tables match. done.");
    Ok(())
}

fn dry_run_counts(sqlite: &SqliteConn) -> Result<()> {
    for table in [
        "projects",
        "tasks",
        "journal_entries",
        "notes",
        "wire_events",
        "messages",
        "links",
        "embeddings",
    ] {
        let n: i64 = sqlite
            .query_row(
                &format!("SELECT COUNT(*) FROM {table}"),
                [],
                |r| r.get(0),
            )
            .unwrap_or(-1);
        if n < 0 {
            println!("  {table:20} (table missing in sqlite)");
        } else {
            println!("  {table:20} rows={n}");
        }
    }
    Ok(())
}

// --- timestamp helpers ------------------------------------------------------

/// Parse sqlite TEXT timestamp ("YYYY-MM-DD HH:MM:SS") as UTC.
///
/// Also tolerates an `Z`/`+00:00` suffix and ISO `T` separators that
/// occasionally show up from python writers.
fn parse_ts(s: &str) -> Result<DateTime<Utc>> {
    let trimmed = s.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(trimmed) {
        return Ok(dt.with_timezone(&Utc));
    }
    let normalized = trimmed.replace('T', " ");
    let bare = normalized.trim_end_matches('Z').trim();
    let formats = ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%d %H:%M:%S", "%Y-%m-%d"];
    for fmt in formats {
        if let Ok(ndt) = NaiveDateTime::parse_from_str(bare, fmt) {
            return Ok(Utc.from_utc_datetime(&ndt));
        }
    }
    Err(anyhow!("unparseable timestamp: {s:?}"))
}

fn parse_ts_opt(s: Option<String>) -> Result<Option<DateTime<Utc>>> {
    match s {
        Some(s) if !s.trim().is_empty() => Ok(Some(parse_ts(&s)?)),
        _ => Ok(None),
    }
}

// --- per-table migrations ---------------------------------------------------

async fn migrate_projects(
    sqlite: &SqliteConn,
    pg: &PgPool,
    skip: bool,
) -> Result<(&'static str, usize, usize)> {
    let mut stmt = sqlite.prepare(
        "SELECT id, name, description, status, owner, created_at, updated_at FROM projects ORDER BY id",
    )?;
    let rows: Vec<(i64, String, Option<String>, String, String, String, String)> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let src = rows.len();

    let sql = if skip {
        "INSERT INTO projects (id, name, description, status, owner, created_at, updated_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$7) ON CONFLICT (id) DO NOTHING"
    } else {
        "INSERT INTO projects (id, name, description, status, owner, created_at, updated_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$7)"
    };
    for (id, name, desc, status, owner, c, u) in rows {
        sqlx::query(sql)
            .bind(id)
            .bind(name)
            .bind(desc)
            .bind(status)
            .bind(owner)
            .bind(parse_ts(&c)?)
            .bind(parse_ts(&u)?)
            .execute(pg)
            .await
            .context("insert projects")?;
    }
    setval(pg, "projects").await?;
    let dst = count_pg(pg, "projects").await?;
    tracing::info!(src, dst, "projects");
    Ok(("projects", src, dst))
}

async fn migrate_tasks(
    sqlite: &SqliteConn,
    pg: &PgPool,
    skip: bool,
) -> Result<(&'static str, usize, usize)> {
    // project column may be NULL post-migration; treat as Option.
    let mut stmt = sqlite.prepare(
        "SELECT id, project, title, body, owner, status, priority, due, block_reason, \
                created_at, updated_at, closed_at FROM tasks ORDER BY id",
    )?;
    #[allow(clippy::type_complexity)]
    let rows: Vec<(
        i64,
        Option<String>,
        String,
        Option<String>,
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        String,
        String,
        Option<String>,
    )> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
                r.get(8)?,
                r.get(9)?,
                r.get(10)?,
                r.get(11)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let src = rows.len();

    let sql = if skip {
        "INSERT INTO tasks (id, project, title, body, owner, status, priority, due, block_reason, created_at, updated_at, closed_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12) ON CONFLICT (id) DO NOTHING"
    } else {
        "INSERT INTO tasks (id, project, title, body, owner, status, priority, due, block_reason, created_at, updated_at, closed_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)"
    };
    for (id, project, title, body, owner, status, prio, due, block, c, u, closed) in rows {
        sqlx::query(sql)
            .bind(id)
            .bind(project)
            .bind(title)
            .bind(body)
            .bind(owner)
            .bind(status)
            .bind(prio)
            .bind(due)
            .bind(block)
            .bind(parse_ts(&c)?)
            .bind(parse_ts(&u)?)
            .bind(parse_ts_opt(closed)?)
            .execute(pg)
            .await
            .context("insert tasks")?;
    }
    setval(pg, "tasks").await?;
    let dst = count_pg(pg, "tasks").await?;
    tracing::info!(src, dst, "tasks");
    Ok(("tasks", src, dst))
}

async fn migrate_journal(
    sqlite: &SqliteConn,
    pg: &PgPool,
    skip: bool,
) -> Result<(&'static str, usize, usize)> {
    let mut stmt = sqlite.prepare(
        "SELECT id, ai, entry_date, title, body, tags, created_at, updated_at \
         FROM journal_entries ORDER BY id",
    )?;
    #[allow(clippy::type_complexity)]
    let rows: Vec<(
        i64,
        String,
        String,
        Option<String>,
        String,
        Option<String>,
        String,
        String,
    )> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let src = rows.len();

    // fts column is GENERATED in postgres ... omit from INSERT.
    let sql = if skip {
        "INSERT INTO journal_entries (id, ai, entry_date, title, body, tags, created_at, updated_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT (id) DO NOTHING"
    } else {
        "INSERT INTO journal_entries (id, ai, entry_date, title, body, tags, created_at, updated_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8)"
    };
    for (id, ai, entry_date, title, body, tags, c, u) in rows {
        sqlx::query(sql)
            .bind(id)
            .bind(ai)
            .bind(entry_date)
            .bind(title)
            .bind(body)
            .bind(tags)
            .bind(parse_ts(&c)?)
            .bind(parse_ts(&u)?)
            .execute(pg)
            .await
            .context("insert journal_entries")?;
    }
    setval(pg, "journal_entries").await?;
    let dst = count_pg(pg, "journal_entries").await?;
    tracing::info!(src, dst, "journal_entries");
    Ok(("journal_entries", src, dst))
}

async fn migrate_notes(
    sqlite: &SqliteConn,
    pg: &PgPool,
    skip: bool,
) -> Result<(&'static str, usize, usize)> {
    let mut stmt = sqlite.prepare(
        "SELECT id, author, title, body, tags, project, created_at, updated_at \
         FROM notes ORDER BY id",
    )?;
    #[allow(clippy::type_complexity)]
    let rows: Vec<(
        i64,
        String,
        Option<String>,
        String,
        Option<String>,
        Option<String>,
        String,
        String,
    )> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let src = rows.len();

    let sql = if skip {
        "INSERT INTO notes (id, author, title, body, tags, project, created_at, updated_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT (id) DO NOTHING"
    } else {
        "INSERT INTO notes (id, author, title, body, tags, project, created_at, updated_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8)"
    };
    for (id, author, title, body, tags, project, c, u) in rows {
        sqlx::query(sql)
            .bind(id)
            .bind(author)
            .bind(title)
            .bind(body)
            .bind(tags)
            .bind(project)
            .bind(parse_ts(&c)?)
            .bind(parse_ts(&u)?)
            .execute(pg)
            .await
            .context("insert notes")?;
    }
    setval(pg, "notes").await?;
    let dst = count_pg(pg, "notes").await?;
    tracing::info!(src, dst, "notes");
    Ok(("notes", src, dst))
}

async fn migrate_wire(
    sqlite: &SqliteConn,
    pg: &PgPool,
    skip: bool,
) -> Result<(&'static str, usize, usize)> {
    let mut stmt = sqlite.prepare(
        "SELECT id, source, category, external_id, title, body, url, severity, affects, \
                acknowledged, pinged_discord, first_seen_at, last_seen_at \
         FROM wire_events ORDER BY id",
    )?;
    #[allow(clippy::type_complexity)]
    let rows: Vec<(
        i64,
        String,
        Option<String>,
        Option<String>,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        i64,
        i64,
        String,
        String,
    )> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
                r.get(8)?,
                r.get(9)?,
                r.get(10)?,
                r.get(11)?,
                r.get(12)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let src = rows.len();

    let sql = if skip {
        "INSERT INTO wire_events (id, source, category, external_id, title, body, url, severity, affects, acknowledged, pinged_discord, first_seen_at, last_seen_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13) ON CONFLICT (id) DO NOTHING"
    } else {
        "INSERT INTO wire_events (id, source, category, external_id, title, body, url, severity, affects, acknowledged, pinged_discord, first_seen_at, last_seen_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)"
    };
    for (id, source, cat, ext, title, body, url, sev, affects, ack, pinged, fs, ls) in rows {
        sqlx::query(sql)
            .bind(id)
            .bind(source)
            .bind(cat)
            .bind(ext)
            .bind(title)
            .bind(body)
            .bind(url)
            .bind(sev)
            .bind(affects)
            .bind(ack != 0)
            .bind(pinged != 0)
            .bind(parse_ts(&fs)?)
            .bind(parse_ts(&ls)?)
            .execute(pg)
            .await
            .context("insert wire_events")?;
    }
    setval(pg, "wire_events").await?;
    let dst = count_pg(pg, "wire_events").await?;
    tracing::info!(src, dst, "wire_events");
    Ok(("wire_events", src, dst))
}

/// messages: not yet in the sqlite schema (post-task-8). Migrate if present,
/// otherwise return zeros so the report stays honest.
async fn migrate_messages(
    sqlite: &SqliteConn,
    pg: &PgPool,
    skip: bool,
) -> Result<(&'static str, usize, usize)> {
    let exists: i64 = sqlite
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='messages'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if exists == 0 {
        tracing::info!("messages table absent in sqlite ... skipping");
        let dst = count_pg(pg, "messages").await.unwrap_or(0);
        return Ok(("messages", 0, dst));
    }

    let mut stmt = sqlite.prepare(
        "SELECT id, sender_ai, recipient_ai, kind, body, in_reply_to, sent_at, read_at \
         FROM messages ORDER BY id",
    )?;
    #[allow(clippy::type_complexity)]
    let rows: Vec<(
        i64,
        String,
        String,
        Option<String>,
        String,
        Option<i64>,
        String,
        Option<String>,
    )> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let src = rows.len();

    let sql = if skip {
        "INSERT INTO messages (id, sender_ai, recipient_ai, kind, body, in_reply_to, sent_at, read_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT (id) DO NOTHING"
    } else {
        "INSERT INTO messages (id, sender_ai, recipient_ai, kind, body, in_reply_to, sent_at, read_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8)"
    };
    for (id, sender, recipient, kind, body, reply, sent, read) in rows {
        sqlx::query(sql)
            .bind(id)
            .bind(sender)
            .bind(recipient)
            .bind(kind)
            .bind(body)
            .bind(reply)
            .bind(parse_ts(&sent)?)
            .bind(parse_ts_opt(read)?)
            .execute(pg)
            .await
            .context("insert messages")?;
    }
    setval(pg, "messages").await?;
    let dst = count_pg(pg, "messages").await?;
    tracing::info!(src, dst, "messages");
    Ok(("messages", src, dst))
}

async fn migrate_links(
    sqlite: &SqliteConn,
    pg: &PgPool,
    skip: bool,
) -> Result<(&'static str, usize, usize)> {
    let mut stmt = sqlite.prepare(
        "SELECT id, source_table, source_id, target_table, target_id, link_type, note, created_at \
         FROM links ORDER BY id",
    )?;
    #[allow(clippy::type_complexity)]
    let rows: Vec<(
        i64,
        String,
        i64,
        String,
        i64,
        Option<String>,
        Option<String>,
        String,
    )> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let src = rows.len();

    let sql = if skip {
        "INSERT INTO links (id, source_table, source_id, target_table, target_id, link_type, note, created_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT (id) DO NOTHING"
    } else {
        "INSERT INTO links (id, source_table, source_id, target_table, target_id, link_type, note, created_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8)"
    };
    for (id, st, sid, tt, tid, lt, note, c) in rows {
        sqlx::query(sql)
            .bind(id)
            .bind(st)
            .bind(sid)
            .bind(tt)
            .bind(tid)
            .bind(lt)
            .bind(note)
            .bind(parse_ts(&c)?)
            .execute(pg)
            .await
            .context("insert links")?;
    }
    setval(pg, "links").await?;
    let dst = count_pg(pg, "links").await?;
    tracing::info!(src, dst, "links");
    Ok(("links", src, dst))
}

async fn migrate_embeddings(
    sqlite: &SqliteConn,
    pg: &PgPool,
    skip: bool,
) -> Result<(&'static str, usize, usize)> {
    let mut stmt = sqlite.prepare(
        "SELECT id, source_table, source_id, model, dim, embedding, content_hash, created_at \
         FROM embeddings ORDER BY id",
    )?;
    #[allow(clippy::type_complexity)]
    let rows: Vec<(i64, String, i64, String, i64, Vec<u8>, String, String)> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get::<_, Vec<u8>>(5)?,
                r.get(6)?,
                r.get(7)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let src = rows.len();

    let sql = if skip {
        "INSERT INTO embeddings (id, source_table, source_id, model, dim, embedding, content_hash, created_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT (id) DO NOTHING"
    } else {
        "INSERT INTO embeddings (id, source_table, source_id, model, dim, embedding, content_hash, created_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8)"
    };
    for (id, st, sid, model, dim, blob, hash, c) in rows {
        let vec = decode_embedding(&blob, dim as usize)
            .with_context(|| format!("decode embedding id={id}"))?;
        let pgv = Vector::from(vec);
        sqlx::query(sql)
            .bind(id)
            .bind(st)
            .bind(sid)
            .bind(model)
            .bind(dim as i32)
            .bind(pgv)
            .bind(hash)
            .bind(parse_ts(&c)?)
            .execute(pg)
            .await
            .context("insert embeddings")?;
    }
    setval(pg, "embeddings").await?;
    let dst = count_pg(pg, "embeddings").await?;
    tracing::info!(src, dst, "embeddings");
    Ok(("embeddings", src, dst))
}

/// LE f32 bytes -> Vec<f32>. python writes via numpy `.tobytes()` little-endian.
fn decode_embedding(blob: &[u8], dim: usize) -> Result<Vec<f32>> {
    if blob.len() != dim * 4 {
        return Err(anyhow!(
            "embedding blob len {} != dim*4 ({})",
            blob.len(),
            dim * 4
        ));
    }
    let mut out = Vec::with_capacity(dim);
    for chunk in blob.chunks_exact(4) {
        out.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    Ok(out)
}

// --- postgres helpers -------------------------------------------------------

async fn setval(pg: &PgPool, table: &str) -> Result<()> {
    let sql = format!(
        "SELECT setval(pg_get_serial_sequence('{table}', 'id'), \
                       (SELECT COALESCE(MAX(id), 0) FROM {table}) + 1, false)"
    );
    sqlx::query(&sql)
        .execute(pg)
        .await
        .with_context(|| format!("setval {table}"))?;
    Ok(())
}

async fn count_pg(pg: &PgPool, table: &str) -> Result<usize> {
    let row = sqlx::query(&format!("SELECT COUNT(*)::bigint AS n FROM {table}"))
        .fetch_one(pg)
        .await
        .with_context(|| format!("count {table}"))?;
    let n: i64 = row.get("n");
    Ok(n as usize)
}
