use anyhow::Result;

use hive_db::queries::links::{EntityRef, LinkSpec, attach_from_specs};
use hive_db::queries::notes;
use hive_db::types::Note;

use crate::cli::{NotesAddArgs, NotesCmd, NotesListArgs, NotesSearchArgs};
use crate::format::{Column, pad_right, print_json, print_table};

pub fn run(cmd: NotesCmd) -> Result<()> {
    let pool = super::pool(false)?;
    let conn = pool.get()?;
    match cmd {
        NotesCmd::Add(args) => add(&conn, args),
        NotesCmd::List(args) => list(&conn, args),
        NotesCmd::Show { id } => show(&conn, id),
        NotesCmd::Search(args) => search(&conn, args),
    }
}

fn add(conn: &hive_db::Connection, args: NotesAddArgs) -> Result<()> {
    let note = notes::add(
        conn,
        args.author,
        args.title.as_deref(),
        &args.body,
        args.project.as_deref(),
        args.tags.as_deref(),
    )?;
    println!("added note #{}", note.id);

    if !args.link.is_empty() {
        let specs = args
            .link
            .iter()
            .map(|s| LinkSpec::parse(s))
            .collect::<hive_db::Result<Vec<_>>>()?;
        let source = EntityRef {
            table: hive_db::enums::LinkTable::Notes,
            id: note.id,
        };
        let msgs = attach_from_specs(conn, &source, &specs)?;
        for m in msgs {
            println!("  {m}");
        }
    }
    Ok(())
}

fn list(conn: &hive_db::Connection, args: NotesListArgs) -> Result<()> {
    let filters = notes::ListFilters {
        author: args.author,
        project: args.project,
        tag: args.tag,
        limit: args.limit,
    };
    let rows = notes::list(conn, &filters)?;

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

fn show(conn: &hive_db::Connection, id: i64) -> Result<()> {
    let row = notes::require(conn, id)?;
    let title = row.title.clone().unwrap_or_else(|| "(untitled)".into());
    println!("#{}  {}  {}", row.id, row.author, title);
    println!("{}", "-".repeat(60));
    let fields: Vec<(&str, String)> = vec![
        ("project", row.project.clone().unwrap_or_default()),
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

fn search(conn: &hive_db::Connection, args: NotesSearchArgs) -> Result<()> {
    if args.hybrid {
        anyhow::bail!(
            "hybrid search is pending (see DESIGN.md embedder section ... task #4 / hive-embed)"
        );
    }
    let _ = (args.author, args.project); // hybrid-only filters
    let hits = hive_db::queries::search::notes(conn, &args.query, args.limit)?;
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
