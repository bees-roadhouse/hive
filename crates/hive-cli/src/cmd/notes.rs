use anyhow::Result;

use crate::api::{self, Note};
use crate::cli::{NotesAddArgs, NotesCmd, NotesListArgs, NotesSearchArgs};
use crate::cmd::links::attach_links;
use crate::format::{Column, fmt_ts_opt, pad_right, print_json, print_table};

pub async fn run(cmd: NotesCmd) -> Result<()> {
    match cmd {
        NotesCmd::Add(args) => add(args).await,
        NotesCmd::List(args) => list(args).await,
        NotesCmd::Show { id } => show(&id).await,
        NotesCmd::Search(args) => search(args).await,
    }
}

async fn add(args: NotesAddArgs) -> Result<()> {
    let note = api::add_note(
        &args.author,
        args.title.as_deref(),
        &args.body,
        args.project.as_deref(),
        args.tags.as_deref(),
    )
    .await?;
    println!("added note #{}", note.id);
    attach_links(&format!("notes:{}", note.id), &args.link).await?;
    Ok(())
}

async fn list(args: NotesListArgs) -> Result<()> {
    let rows = api::list_notes(
        args.author.as_deref(),
        args.project.as_deref(),
        args.tag.as_deref(),
        args.limit,
    )
    .await?;

    if args.json {
        print_json(&rows)?;
        return Ok(());
    }
    if rows.is_empty() {
        println!("no notes");
        return Ok(());
    }
    let cols: Vec<Column<'_, Note>> = vec![
        Column::new("id", |n: &Note| n.id.to_string()),
        Column::new("author", |n: &Note| n.author.clone()),
        Column::new("project", |n: &Note| n.project.clone().unwrap_or_default()),
        Column::new("title", |n: &Note| n.title.clone().unwrap_or_default()),
    ];
    let trailing: Box<dyn Fn(&Note) -> String> =
        Box::new(|n| n.tags.clone().unwrap_or_default());
    print_table(&cols, &rows, Some(("tags", trailing)));
    Ok(())
}

async fn show(id: &str) -> Result<()> {
    let row = api::show_note(id).await?;
    let title = row.title.clone().unwrap_or_else(|| "(untitled)".into());
    println!("#{}  {}  {}", row.id, row.author, title);
    println!("{}", "-".repeat(60));
    let fields: Vec<(&str, String)> = vec![
        ("project", row.project.clone().unwrap_or_default()),
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

async fn search(args: NotesSearchArgs) -> Result<()> {
    if args.hybrid {
        anyhow::bail!(
            "hybrid search is pending (hive-api /search/semantic returns 501; task #4 / hive-embed)"
        );
    }
    let _ = (args.author, args.project); // hybrid-only filters
    let hits = api::search_notes(&args.query, args.limit).await?;
    if hits.is_empty() {
        println!("no matches");
        return Ok(());
    }
    for h in &hits {
        let title = h.title.clone().unwrap_or_else(|| "(untitled)".into());
        let proj = h.project.as_deref().map(|p| format!(" [{p}]")).unwrap_or_default();
        println!("#{}  {}{}  {}", h.id, h.author, proj, title);
        println!("    {}", h.snippet);
    }
    Ok(())
}
