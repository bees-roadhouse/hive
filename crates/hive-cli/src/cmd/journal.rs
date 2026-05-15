use anyhow::Result;
use chrono::Local;

use hive_db::queries::journal;
use hive_db::queries::links::{EntityRef, LinkSpec, attach_from_specs};
use hive_db::types::JournalEntry;

use crate::cli::{JournalAddArgs, JournalCmd, JournalListArgs, JournalSearchArgs};
use crate::format::{Column, pad_right, print_json, print_table};

pub fn run(cmd: JournalCmd) -> Result<()> {
    let pool = super::pool(false)?;
    let conn = pool.get()?;
    match cmd {
        JournalCmd::Add(args) => add(&conn, args),
        JournalCmd::List(args) => list(&conn, args),
        JournalCmd::Show { id } => show(&conn, id),
        JournalCmd::Search(args) => search(&conn, args),
    }
}

fn add(conn: &hive_db::Connection, args: JournalAddArgs) -> Result<()> {
    let entry_date = args
        .date
        .clone()
        .unwrap_or_else(|| Local::now().date_naive().format("%Y-%m-%d").to_string());
    chrono::NaiveDate::parse_from_str(&entry_date, "%Y-%m-%d").map_err(|_| {
        hive_db::Error::InvalidFormat {
            field: "--date",
            value: entry_date.clone(),
            expected: "YYYY-MM-DD",
        }
    })?;
    let entry = journal::add(
        conn,
        args.ai,
        &entry_date,
        args.title.as_deref(),
        &args.body,
        args.tags.as_deref(),
    )?;
    println!("added journal entry #{} ({} {})", entry.id, entry.ai, entry.entry_date);

    if !args.link.is_empty() {
        let specs = args
            .link
            .iter()
            .map(|s| LinkSpec::parse(s))
            .collect::<hive_db::Result<Vec<_>>>()?;
        let source = EntityRef {
            table: hive_db::enums::LinkTable::JournalEntries,
            id: entry.id,
        };
        let msgs = attach_from_specs(conn, &source, &specs)?;
        for m in msgs {
            println!("  {m}");
        }
    }
    Ok(())
}

fn list(conn: &hive_db::Connection, args: JournalListArgs) -> Result<()> {
    if let Some(d) = &args.from_date {
        chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").map_err(|_| {
            hive_db::Error::InvalidFormat {
                field: "--from",
                value: d.clone(),
                expected: "YYYY-MM-DD",
            }
        })?;
    }
    if let Some(d) = &args.to_date {
        chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").map_err(|_| {
            hive_db::Error::InvalidFormat {
                field: "--to",
                value: d.clone(),
                expected: "YYYY-MM-DD",
            }
        })?;
    }
    let filters = journal::ListFilters {
        ai: args.ai,
        from_date: args.from_date,
        to_date: args.to_date,
        tag: args.tag,
        limit: Some(args.limit),
    };
    let rows = journal::list(conn, &filters)?;

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
        Column::new("title", |r: &JournalEntry| r.title.clone().unwrap_or_default()),
    ];
    let trailing: Box<dyn Fn(&JournalEntry) -> String> =
        Box::new(|r| r.tags.clone().unwrap_or_default());
    print_table(&cols, &rows, Some(("tags", trailing)));
    Ok(())
}

fn show(conn: &hive_db::Connection, id: i64) -> Result<()> {
    let row = journal::require(conn, id)?;
    let title = row.title.clone().unwrap_or_else(|| "(untitled)".into());
    println!("#{}  {}  {}  {}", row.id, row.entry_date, row.ai, title);
    println!("{}", "-".repeat(60));
    let fields: Vec<(&str, String)> = vec![
        ("tags", row.tags.clone().unwrap_or_default()),
        ("created_at", row.created_at.clone()),
        ("updated_at", row.updated_at.clone()),
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

fn search(conn: &hive_db::Connection, args: JournalSearchArgs) -> Result<()> {
    if args.hybrid {
        anyhow::bail!(
            "hybrid search is pending (see DESIGN.md embedder section ... task #4 / hive-embed)"
        );
    }
    let _ = args.ai; // hybrid-only filter
    let hits = hive_db::queries::search::journal(conn, &args.query, args.limit)?;
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
