use anyhow::Result;

use crate::api::{self, JournalEntry};
use crate::cli::{JournalAddArgs, JournalCmd, JournalListArgs, JournalSearchArgs};
use crate::cmd::links::attach_links;
use crate::format::{Column, fmt_ts_opt, pad_right, print_json, print_table};

pub async fn run(cmd: JournalCmd) -> Result<()> {
    match cmd {
        JournalCmd::Add(args) => add(args).await,
        JournalCmd::List(args) => list(args).await,
        JournalCmd::Show { id } => show(&id).await,
        JournalCmd::Search(args) => search(args).await,
    }
}

async fn add(args: JournalAddArgs) -> Result<()> {
    if let Some(d) = &args.date {
        chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d")
            .map_err(|_| anyhow::anyhow!("invalid --date '{d}'. expected YYYY-MM-DD"))?;
    }
    let entry = api::add_journal(
        &args.ai,
        args.date.as_deref(),
        args.title.as_deref(),
        &args.body,
        args.tags.as_deref(),
    )
    .await?;
    println!(
        "added journal entry #{} ({} {})",
        entry.id, entry.ai, entry.entry_date
    );
    attach_links(&format!("journal:{}", entry.id), &args.link).await?;
    Ok(())
}

async fn list(args: JournalListArgs) -> Result<()> {
    if let Some(d) = &args.from_date {
        chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d")
            .map_err(|_| anyhow::anyhow!("invalid --from '{d}'. expected YYYY-MM-DD"))?;
    }
    if let Some(d) = &args.to_date {
        chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d")
            .map_err(|_| anyhow::anyhow!("invalid --to '{d}'. expected YYYY-MM-DD"))?;
    }
    let rows = api::list_journal(
        args.ai.as_deref(),
        args.from_date.as_deref(),
        args.to_date.as_deref(),
        args.tag.as_deref(),
        args.limit,
    )
    .await?;

    if args.json {
        print_json(&rows)?;
        return Ok(());
    }
    if rows.is_empty() {
        println!("no journal entries");
        return Ok(());
    }

    let cols: Vec<Column<'_, JournalEntry>> = vec![
        Column::new("id", |r: &JournalEntry| r.id.to_string()),
        Column::new("date", |r: &JournalEntry| r.entry_date.clone()),
        Column::new("ai", |r: &JournalEntry| r.ai.clone()),
        Column::new("title", |r: &JournalEntry| {
            r.title.clone().unwrap_or_default()
        }),
    ];
    let trailing: Box<dyn Fn(&JournalEntry) -> String> =
        Box::new(|r| r.tags.clone().unwrap_or_default());
    print_table(&cols, &rows, Some(("tags", trailing)));
    Ok(())
}

async fn show(id: &str) -> Result<()> {
    let row = api::show_journal(id).await?;
    let title = row.title.clone().unwrap_or_else(|| "(untitled)".into());
    println!("#{}  {}  {}  {}", row.id, row.entry_date, row.ai, title);
    println!("{}", "-".repeat(60));
    let fields: Vec<(&str, String)> = vec![
        ("tags", row.tags.clone().unwrap_or_default()),
        ("created_at", fmt_ts_opt(&row.created_at)),
        ("updated_at", fmt_ts_opt(&row.updated_at)),
    ];
    let label_w = fields.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    for (k, v) in &fields {
        println!("  {}  {}", pad_right(k, label_w), v);
    }
    println!();
    println!("body:");
    println!("{}", row.body);
    Ok(())
}

async fn search(args: JournalSearchArgs) -> Result<()> {
    if args.hybrid {
        anyhow::bail!(
            "hybrid search is pending (hive-api /search/semantic returns 501; task #4 / hive-embed)"
        );
    }
    let _ = args.ai; // hybrid-only filter
    let hits = api::search_journal(&args.query, args.limit).await?;
    if hits.is_empty() {
        println!("no matches");
        return Ok(());
    }
    for h in &hits {
        let title = h.title.clone().unwrap_or_else(|| "(untitled)".into());
        println!("#{}  {}  {}  {}", h.id, h.entry_date, h.ai, title);
        println!("    {}", h.snippet);
    }
    Ok(())
}
